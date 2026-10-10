# Third-party plugin upstream tracking

One row per Jellyfin plugin ported into Ferrofin as a compiled-in extension
(`crates/ferrofin-extensions/`). This is the single source of truth for *which
upstream revision each port is based on*.

Syncing with upstream: clone the upstream repo, `git log <ported rev>..HEAD`, port each
behavioural change (or classify it as not-applicable / accepted divergence in the notes
below), then bump **Ported rev** and the matching asset pin.

Conventions:
- **Ported rev** — the upstream commit the Ferrofin implementation is faithful to.
  Bump it only after the delta is actually ported (or classified as
  not-applicable/accepted-divergence below).
- Dashboard-asset pins live in `crates/ferrofin-extensions/build.rs`
  (`*_REPO`/`*_REV` consts) and must be kept equal to **Ported rev**.

| Plugin | Upstream repo | Ported rev | Upstream version | Status |
|---|---|---|---|---|
| Intro Skipper | https://github.com/intro-skipper/intro-skipper | `db09359` | 10.11/prerelease | ported |
| File Transformation | https://github.com/IAmParadox27/jellyfin-plugin-file-transformation | `f4f01c3` | 2.5.10.0 | ported |
| Merge Versions | https://github.com/danieladov/jellyfin-plugin-mergeversions | `e6f58d6` | 12.0.0 | ported |

## Per-plugin notes

### Intro Skipper
- Ferrofin files:
  - the extension: `crates/ferrofin-extensions/src/intro_skipper.rs` (tasks,
    config, caches, the analysis seam) and `intro_skipper/` —
    `queue.rs` (`QueueManager`), `engine.rs` (`BaseItemAnalyzerTask`,
    `ChromaprintAnalyzer`, `TimeAdjustmentHelper`), `chapter_analyzer.rs`,
    `black_frame.rs` (`BlackFrameAnalyzer`, recaps), `credit_scenes.rs`
    (`CreditsBlackFrameAnalyzer` and `Analyzers/Credits`), `automatic.rs`
    (`Entrypoint`), `config_hash.rs` (`ConfigHasher`), test data `testdata/`;
  - processes: `crates/ferrofin-extensions/src/ffmpeg.rs` (`FFmpegService`,
    `FFmpegOutputParser`), `fingerprint.rs` (Chromaprint);
  - storage: `crates/ferrofin-core/src/intro_skipper_repository.rs`, migrations
    `0036`–`0038` (`Ferrofin*` tables), the seam
    `crates/ferrofin-traits/src/intro_skipper.rs`;
  - routes: `crates/ferrofin-api/src/handlers/intro_skipper.rs`, the
    first-episode filter in `handlers/media_segments.rs`; tests
    `crates/ferrofin-api/tests/intro_skipper_handlers.rs`;
  - vendored assets `crates/ferrofin-extensions/assets/introskipper/`.
- Ported: the segment store and its routes, the queue, incremental analysis
  under `ConfigHasher`, settled-season reanalysis, every analyzer (Chromaprint,
  chapters, black frames, credit scenes, recaps, anime previews), silence and
  keyframe snapping, `ProbeAudioDuration`, `ProcessPriority`/`ProcessThreads`,
  the detection cache, the Clean Intro Skipper Cache task, automatic analysis
  (`Entrypoint`) and `AnalyzeAgain`, `MediaSegmentsFirstEpisodeFilter`, the task
  identities and the support bundle. The upstream xUnit suites are
  transliterated beside the code they test.
- Accepted divergences (do NOT "fix" during sync):
  - **Storage:** the plugin's own database is not imported (owner decision D3);
    Ferrofin keeps its segments in `FerrofinIntroSkipperSegments` and publishes
    them to `MediaSegments` under the plugin's MD5 provider id (D1, D2). The
    detection cache is files beside the fingerprints (D7), not SQLite: prints
    are keyed by window in whole seconds without a settings hash (re-keying
    would re-fingerprint every library; a sub-second window change reuses the
    old print), and the clean task deletes cache files no current read hits.
    Migration `0037` seeds the segment store from the segments already
    published, as automatic rows with an empty hash (upstream's schema upgrade
    does the same): the first pass replaces each with what it detects, or
    deletes it. Hand edits made before `0037`, and the plugin's own records of
    them on an adopted database, are therefore not kept; nor are the plugin's
    settings (`IntroSkipper.xml`). `docs/UPGRADING.md` tells operators.
  - **Hashes:** Ferrofin's analysis hashes carry an analyzer-set token
    (`config_hash::ANALYZERS`), bumped when an analyzer lands, so earlier
    results are analysed again with it.
  - **Fingerprinting:** ffmpeg's `chromaprint` muxer, falling back to `fpcalc`
    (upstream has only the muxer); `IncompatibleFFmpegBuild` is raised only
    when neither exists.
  - **Upstream bugs not ported:** an anime preview is stored under the Preview
    hash (upstream's Credits hash lets the Preview pass delete it for good);
    Matroska `DURATION` tags parse at any fraction length (.NET's `TimeSpan`
    stops at seven digits).
  - **Failures:** a store write failure fails the season, retried next pass,
    in every analyzer (upstream's black-frame analyzers log it per episode and
    go on, the episode left analysed in memory); a failed keyframe
    detection keeps the end; an invalid or runaway chapter pattern matches
    nothing, reported once per pattern (upstream fails the pass); a negative
    adjustment window is logged once, not per segment.
  - **Concurrency:** the clean task holds the one-pass latch (upstream takes no
    lock and can delete segments a concurrent pass just stored).
  - **Automatic analysis:** library changes arrive through the event bus (D8),
    debounced by `LibraryUpdateDuration` and batched, so a batch with any
    addition waits as an addition, and image-only updates are not told apart.
  - **Media Segment Scan** (`TaskExtractMediaSegments`) runs detection (D5).
  - **Support bundle:** the version line also names Ferrofin's; the ffmpeg
    checks run when the bundle is asked for (upstream: at start); the bundle
    names the Chromaprint backend.
  - **Task keys:** the detection task was `IntroSkipper.Detect` before D6; the
    task manager adopts what was saved under that key.

### File Transformation
- Ferrofin files: `crates/ferrofin-extensions/src/file_transformation.rs`,
  vendored assets `crates/ferrofin-extensions/assets/filetransformation/`.

### Merge Versions
- Ferrofin files: `crates/ferrofin-extensions/src/merge_versions.rs` (the whole
  plugin — `MergeVersionsExtension`, `MergeVersionsService`, both scheduled
  tasks, config, eligibility filters), vendored assets
  `crates/ferrofin-extensions/assets/mergeversions/`, the trait seam
  `crates/ferrofin-traits/src/merge_versions.rs`, and the thin HTTP handlers
  `crates/ferrofin-api/src/handlers/merge_versions.rs`. The single-group
  `merge_versions`/`remove_alternate_sources` core ops (backing
  `POST /Videos/MergeVersions` + `DELETE /Videos/{id}/AlternateSources`) stay
  in `crates/ferrofin-core/src/library_manager.rs` — those are core Jellyfin
  routes, not plugin surface.
- Ported at 12.0 semantics (`e6f58d6`): provider-first episode merge key
  (Tvdb→Tmdb→Imdb → numbers → title, case-insensitive), transitive
  version-group expansion, existing-primary-preserving primary selection,
  `LocationsExcluded` config + inactive-library eligibility filters, the two
  24-hour dashboard tasks, and the vendored settings page. Routes and tasks
  self-gate on the plugin's enabled flag (disabled → routes 404, tasks no-op).
- Divergence PENDING OWNER DECISION (D10 in
  `brain/plans/PLAN_ITEM_FILE_DELETION.md`): the pinned plugin (`e6f58d6`,
  `MergeVersionsManager.cs:211-233`) writes a merge as OWNED local versions —
  each alternate gets `OwnerId` = the primary and the primary's
  `LocalAlternateVersions` lists it, so the save writes `ChildType` 2 rows —
  and its split (`:282-299`) unlinks local versions too. Ferrofin writes a
  merge as a linked version (`PrimaryVersionId` plus a `ChildType` 3 row, as
  core `VideosController.MergeVersions` does), never merges or splits an
  owned row, and leaves `OwnerId`/type-2 rows to the scanner, which owns the
  files it groups (`ItemPersistenceService::sync_local_versions`). An
  adopted database's plugin merges (owned, type 2) therefore read as local
  versions here. The linked-child reroute is not ported.
- Accepted divergences (do NOT "fix" during sync): No
  `VideoType`/`Video3DFormat` columns in Ferrofin's schema, so primary selection
  cannot demote 3D/non-file videos (width ordering only). No `IndexNumberEnd`
  column, so that episode-key component is always empty. Upstream's
  `Parallel.ForEach` fire-and-forget async merges (error-dropping) run
  sequentially in Ferrofin. The episode merge key is scoped to the series
  *row* (`SeriesPresentationUniqueKey`), not the series name as upstream
  does: a show present in two libraries (hot/cold tiers) has two series rows
  with one name, and the name-scoped key merged episodes across them —
  hiding each alternate from its own series' list and skewing season counts.
  The bulk episode task self-heals links whose key no longer matches their
  primary's (unlink + regroup within the series). For the same reason movie
  grouping is keyed by (owning library `TopParentId`, `Tmdb` id) rather than
  the `Tmdb` id alone: the same film held in a cold (NAS) and a hot (local)
  library is two intentional entries, and merging them hid one behind the
  other. The bulk movie task self-heals existing cross-library links between
  `Tmdb`-carrying movies, and a group's `GetAllAlternateVersions` expansion
  drops any member it reaches outside that group's library, so a partner that
  never reaches the scan (excluded location, inactive library) cannot be
  re-merged through a stale pointer; such a member is unlinked only when it
  points back into the scanned library, never when its own primary is also
  outside (that is the other library's group, which this scan could not
  repair). Accepted cost, deliberately not configurable: the
  separate-4K-library setup that upstream's Tmdb-only key served — one item
  offering the HD and 4K copies as versions — no longer merges, and a
  cross-library group made by hand via `POST /Videos/MergeVersions` (which
  is itself unscoped, by design) is undone by the next nightly run whenever
  both copies carry a Tmdb id.
