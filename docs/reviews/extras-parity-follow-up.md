# Extras parity follow-up (#32)

Reference: Jellyfin 12.1, commit `ee91c75e777da41a9c4f4855e70adc604fbf2ef8`.
This follows [the initial client visibility audit](extras-client-visibility.md).

## Changes

- `1a15bedf`: directories resolved as multiple movie files drop unrecognized
  sample filenames. Explicit Sample extras and individual-file fallback remain.
  Full and scoped scans remove the corresponding legacy database rows.
- `1b113e7e`: DTO counts and specials, trailers and theme endpoints share batched
  owner resolution. Video owners include the primary, linked versions and those
  versions' local alternates. Series owners include matching presentation keys
  only when automatic series grouping is enabled in the current library options.
  Queries use existing indexes and bounded input batches; no schema change.

- `bfd25df4`: music-video libraries resolve MusicVideo items with filename-based
  names and unparsed names during owner grouping. Legacy Movie rows and adopted
  generic Video versions retain their IDs, play history and extras relationships.
  When both kinds share a path, identity and ownership use the same priority.
- `cde66711`: series and physical-season folders discover specials, trailers and
  themes before episode resolution. Normal and scoped scans repair legacy
  Episodes, preserve their IDs and metadata, and remove bogus extras Seasons.
  Extra names, localized type labels, numbering and mixed-folder flags follow
  Jellyfin. Failed listings protect existing rows; explicit Name locks survive.
- `ac1bac4a`: test fixtures use the persistence APIs, retaining the SQL boundary
  ratchet without raising any ceilings.

## Live results

Disposable libraries were scanned by each server. Two movies were merged through
`POST /Videos/MergeVersions`; one series occupied two library locations.

| Request | Jellyfin 12.1 | Ferrofin |
|---|---:|---:|
| Either merged movie: specials / trailers | 2 / 2 | 2 / 2 |
| Either merged movie: theme songs / videos | 3 / 3 | 3 / 3 |
| Either grouped series folder: specials / trailers | 2 / 2 | 2 / 2 |
| Either grouped series folder: theme songs / videos | 1 / 1 | 1 / 1 |
| Episode themes, inheritance enabled | 1 / 1 | 1 / 1 |
| Episode themes, inheritance disabled | 0 / 0 | 0 / 0 |

Counts matched the specials/trailers endpoint lengths. Every returned extra had
playable media sources. The TV discovery fix was present for these checks.

Jellyfin Web in Chromium also passed these checks:

- Both theme settings disabled: no playback.
- Theme songs enabled: audio playback and advancement to another theme.
- Both enabled: video selected, then advancement to another video theme.
- Episode detail: inherited series audio played.

The audio player streams directly without a PlaybackInfo request. The browser
harness therefore checks actual media `playing` events and distinct stream paths.

## Documentation compatibility

The 34-case reference audit for music-video libraries found the same four
exceptions as the movie audit: standalone `sample.mkv`, `.sample`, and plain-space
`sample` are filtered; plain-space `trailer` is a regular library item. The
implementation targets Jellyfin runtime behavior where documentation differs.

## Review and validation

Each implementation phase received independent review through the repository's
[review-loop skill](../../.claude/skills/review-loop/SKILL.md). The TV review
continued past three rounds with explicit user approval. All findings were
resolved; final reviews approved the changes.

- Scanner extras: 18 tests passed, including full/scoped repair, unavailable
  directories, bogus-season removal, Name locks, play history, stable adopted IDs,
  mixed-media owner checks and music-video version grouping with differing years.
- Movie audit: 34/34 recorded entries match Jellyfin (type, ownership, counts,
  extras endpoints and themes). Download hashes and playback sources checked.
- Music-video audit: 34/34 recorded entries match Jellyfin. A separate dated-file
  probe confirmed that `Artist - Song (2020).mkv` retains that name and gets year
  2020 during Jellyfin's metadata refresh.
- TV audit: all nine persisted rows match the reference, including rejection of
  `Extras/theme.mp3` and an unrelated trailer under `theme-music/`.
- API coverage: 86.20% lines; 756 tests passed.
- Core coverage: 94.16% lines; 1,915 tests passed. LLVM reports 17 functions with
  mismatched profile data, as in the earlier coverage audit. The command exits
  successfully and exceeds the 80% gate; this is not a warning-free report.
- Workspace regression: 7,382 passed, 5 skipped. Doctests: 3 passed.
- Workspace build, `cargo fmt --all --check`, and workspace Clippy with all targets,
  all features and `-D warnings` passed. Final lint-only correction: `316754f6`.
- No migrations, adoption-gate changes or boot repairs were added. Tests cover
  adopted item identities; the seven-path migration adoption matrix is unchanged.

## Before/after latency

Debug binaries, identical disposable database copies, three alternating runs,
100 requests per endpoint after 10 warmups. Server affinity was pinned to two
initially idle physical cores (movie run: CPUs 7/22; series run: CPUs 6/7).
Baseline includes the earlier client-count fix. These measurements cover HTTP
code unchanged by the later scanner-only review corrections.

| Request | Before median p50 | After median p50 | Range across runs, before / after |
|---|---:|---:|---|
| Movie detail | 2.30 ms | 2.51 ms | 2.28–2.34 / 2.51–2.56 ms |
| 100 movies with extras counts | 11.87 ms | 12.66 ms | 11.61–11.92 / 12.65–12.88 ms |
| 100 movies without extras counts | 11.01 ms | 10.89 ms | 10.85–11.02 / 10.77–11.89 ms |
| Grouped-series detail | 1.95 ms | 2.84 ms | 1.91–1.99 / 2.81–2.94 ms |
| Grouped-series themes | 3.75 ms | 6.29 ms | 3.69–3.77 / 6.19–6.31 ms |

Series requests now read current library grouping options and include extras from
both folders. That adds work; this fixture has two libraries. The options lookup
cost grows with configured libraries. No new database indexes or query-plan
changes outside owner aggregation were introduced.

## Rechecking on a clean library

Use a disposable test library with a real short playable file copied into the
standard extras folders. Check the movie detail cards and trailer control, then
play them. For series, place extras at both series and physical-season levels;
regular episode counts must stay unchanged. Test merged versions from either
version and a grouped series from either folder. Counts should match endpoint
lengths. Enable theme audio/video in Jellyfin Web's display preferences to test
playback; both settings are disabled by default. Rescan twice and confirm IDs and
play history are retained, then remove the disposable library.
