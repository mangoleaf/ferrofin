# Provider identity, field mapping and artwork gaps

Branch: `fix/movie-metadata-23`, based on `scan-change-detection` at `71311ef2`.
This follows the shared-key, subtitle and cast-image fixes on the same branch.
The parent worktree's concurrent provider-chain changes were left untouched.

## Changes

TVDB now resolves a series by its native id, then IMDb, Zap2It and TMDB ids,
before considering its name. A supplied external id that resolves to nothing
does not fall through to a potentially different series with a similar title.
The scanner and Identify both use the authenticated `/search/remoteid/{id}`
endpoint. Movie/person results cannot become series matches.

These provider fields now reach the stored metadata:

| Provider | Added mappings |
|---|---|
| TMDB movie/series | Name, original title and language; keywords; movie production countries and collection name/id; series runtime, last air date, home page, TVDB and TVRage ids |
| TMDB episode | IMDb, TVDB and TVRage ids and trailers, fetched together with credits in one detail request |
| OMDb | English title, original language and home page |
| TVDB series | Name, original title, runtime, slug, official collection ids and Zap2It id; TMDB ids have the optional title suffix removed |
| TVDB episode | Original title, episode TVDB/IMDb ids and the special's before/after season/episode placement |

Home pages and collection names use the existing `BaseItems.Data` JSON, with
normal fill/replace rules. There is no schema change. Movie runtime still comes
from the media probe; series runtime comes from provider episode-duration data.
The episode DTO now reads special placement when `SpecialEpisodeNumbers` is
requested, and also returns a stored `IndexNumberEnd`.

Fanart leads artwork by default for movies, series and music. Explicit library
or server artwork order takes precedence, and disabled providers remain disabled.
The Choose Image listing uses the same order. Existing files are reused unless
replacement was requested. Failed preferred downloads can fall through to the
next candidate; if all fail, an existing image survives. Only one image row of
each type is returned.

## Upstream references

Compared against Jellyfin `96bca6f0bd`, TVDB plugin `5c4592f` and Fanart plugin
`7c2f859`, using the local source checkouts and the official TVDB API schema:

- [TMDB movie mappings](https://github.com/jellyfin/jellyfin/blob/96bca6f0bd/MediaBrowser.Providers/Plugins/Tmdb/Movies/TmdbMovieProvider.cs)
- [TMDB series mappings](https://github.com/jellyfin/jellyfin/blob/96bca6f0bd/MediaBrowser.Providers/Plugins/Tmdb/TV/TmdbSeriesProvider.cs)
- [TMDB episode mappings](https://github.com/jellyfin/jellyfin/blob/96bca6f0bd/MediaBrowser.Providers/Plugins/Tmdb/TV/TmdbEpisodeProvider.cs)
- [OMDb mappings](https://github.com/jellyfin/jellyfin/blob/96bca6f0bd/MediaBrowser.Providers/Plugins/Omdb/OmdbProvider.cs)
- [TVDB series identity and mappings](https://github.com/jellyfin/jellyfin-plugin-tvdb/blob/5c4592f/Jellyfin.Plugin.Tvdb/Providers/TvdbSeriesProvider.cs)
- [TVDB episode mappings](https://github.com/jellyfin/jellyfin-plugin-tvdb/blob/5c4592f/Jellyfin.Plugin.Tvdb/Providers/TvdbEpisodeProvider.cs)
- [TVDB API schema](https://github.com/thetvdb/v4-api/blob/master/docs/swagger.yml)

Fanart's series order is 1 and TMDB's is 2 upstream; Fanart also precedes
AudioDB for music. The inspected Jellyfin movie providers instead declare TMDB
order 0 and Fanart order 1. This change uses Fanart first for movies as requested,
while retaining library overrides.

## Validation

Tests cover authenticated remote-id lookup, unrelated/no-match responses,
Identify without a title, field decoding and persistence, unchanged answers,
configured artwork order, download fallback and preservation of existing files.

The real HTTP scan matrix adds a movie with collection/keyword/country data and
a TVDB-only library with an IMDb-pinned series. It checks DTO fields, saved ids,
Fanart downloads, a quiet rescan, and a library order override during image
replacement. Existing cancellation, subtitle, metadata outage and unchanged cast
image checks remain part of the regression suites.

Final checks, using `RUSTC_WRAPPER=` and this worktree's isolated target:

- Core coverage run: 1,831 tests passed; **93.95%** line coverage.
- Provider coverage run: 583 tests passed, 4 skipped; **92.54%** line coverage.
- Real HTTP scan matrix: passed, including all previous subtitle, outage and
  unchanged cast-image checks (18.85 seconds total).
- Clippy for core, providers and server, all targets/features, `-D warnings`:
  passed. Formatting and `git diff --check`: passed.
- The full workspace suite was not run.

Coverage was gated separately at 80%, with linked dependency sources excluded:

```sh
RUSTC_WRAPPER= cargo llvm-cov nextest -p ferrofin-providers --summary-only \
  --ignore-filename-regex 'ferrofin-(common|db|model|naming|traits|util)/' \
  --fail-under-lines 80
RUSTC_WRAPPER= cargo llvm-cov nextest -p ferrofin-core --summary-only \
  --ignore-filename-regex 'ferrofin-(common|db|drawing|keyframes|model|naming|networking|providers|traits|util)/' \
  --fail-under-lines 80
```

## Performance

The scan benchmark used 300 movies and 10 series of 10 episodes (420 items),
without ffprobe or remote providers. Four runs per build, alternating before and
after, with no concurrent validation jobs. The before binary is from `8994e95d`;
the after binary contains this change. Outcomes matched in every run.

| Median wall time | Before | After |
|---|---:|---:|
| First scan | 1.1130 s | 1.1440 s |
| Rescan | 0.7305 s | 0.7065 s |

The debug timing ranges overlap. These numbers measure local scan overhead, not
provider latency. The episode regression separately verifies one detail/credits
request per changed episode and no request on an unchanged rescan.

```sh
FERROFIN_SCAN_BENCH=1 FERROFIN_SCAN_BENCH_MOVIES=300 \
FERROFIN_SCAN_BENCH_SERIES=10 FERROFIN_SCAN_BENCH_EPISODES=10 \
RUSTC_WRAPPER= cargo test -p ferrofin-server --test scan_bench -- --nocapture
```

Session artifacts: `/tmp/ferrofin-metadata-bench.log` and
`/tmp/ferrofin-metadata-bench.json`; coverage reports are
`/tmp/metadata-core-coverage.log` and `/tmp/metadata-provider-coverage.log`.

## Integration with the final scan PR

The metadata branch was created from `feat/scan-change-detection` at `71311ef2`.
PR #21 was subsequently squash-merged into main as `670d530e`, including scan
changes made after that fork. Git's ordinary merge base therefore predates the
scan work and reports many files as independently added.

The merge resolution uses `71311ef2` to compare the actual changes on each side.
It preserves all 13 files changed only on main byte-for-byte, and integrates the
metadata/subtitle changes with main's final scan behavior. In particular, TMDB's
combined episode detail/credits request retains billing order and the rule that
an empty TMDB credit list does not clear existing people; TVDB retains its
provider-order-sensitive credit reset behavior.

Validation of the merged tree:

- 2,795 tests passed across core, providers, database and traits; 5 skipped.
- Separate coverage runs passed: core 94.07% (1,884 tests), providers 92.65%
  (716 tests, 4 skipped), each gated at 80% with dependency sources excluded.
- Real HTTP scan matrix passed in 18.13 seconds, including subtitle downloads,
  provider identities/artwork, outage behavior and unchanged cast-image rows.
- All 33 verification-script tests passed with local HTTP fixtures enabled.
- Clippy (core/providers/server, all targets/features, warnings denied), formatting
  and whitespace checks passed.

Four alternating runs per build of the same 420-item scan fixture described
above compared `54542417` with the merged tree. Median first-scan time was
1.1295 → 1.1210 seconds; median rescan time was 0.7155 → 0.7150 seconds.
Timing ranges overlap and scan outcomes match. These debug measurements cover
local scan overhead without ffprobe or remote providers. Raw results are in
`/tmp/metadata-merge-bench.log` and `/tmp/metadata-merge-bench.json`.
