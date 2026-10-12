# Extras client visibility and documentation audit (#32)

Date: 2026-10-01. Branch: `fix/extras-library-browse`.
Count fix: `c9b9f854`.

## Result

The browse fix retained playable owned extras, but movie details still reported
`SpecialFeatureCount: 0` and `LocalTrailerCount: 0`. Jellyfin web uses these fields
to display special features and to select local trailers for playback. The
previous validation checked storage and extras endpoints but missed this client
contract.

The follow-up replaces those constants with counts from owned rows. Counts use
Jellyfin's display-extra types, include Unknown extras, and exclude theme media,
NULL types, and invalid types. Both counts are read together in batches of at
most 500 owner ids, only when requested. The query uses the existing OwnerId
index. No migration or scan is needed to populate the counts for existing owned
extras. This does not change discovery or owner aggregation across versions.

`ExtraCounts` and `ItemCountService::get_extra_counts_batch` carry the result
through the existing dependency-injection boundary. The DTO reads the page's
prefetched map. No SQL was added to the DTO service.

## Documentation parity

**Full parity with the documentation is not established; confirmed gaps remain.**
The [official extras section](https://jellyfin.org/docs/general/server/media/movies/#extras)
was checked against a live Jellyfin 12.1 server (source commit
`ee91c75e777da41a9c4f4855e70adc604fbf2ef8`) and the fixed Ferrofin build.

A disposable movie library contains 34 separate cases: 13 extra folders, two
standalone filenames, 18 suffixes, and a standalone theme song. Both servers
recognize the same 30 cases as owned extras. Those include every documented
folder and both forms of theme songs, plus theme videos. Counts agree with the
SpecialFeatures and LocalTrailers responses in all 34 cases. Theme counts are
also compared. Recognized extras have playback sources; downloaded bytes match
the source file's SHA-256.

| Documented case | Jellyfin 12.1 | Ferrofin |
| --- | --- | --- |
| `sample.mkv` | Ignored | Ignored |
| `Bonus.sample.mkv` | Ignored | Ignored |
| `Bonus trailer.mkv` | Separate movie | Separate movie |
| `Bonus sample.mkv` | Ignored | Separate movie |

These four layouts do not behave as the documentation describes in the tested
Jellyfin release. The last row is also a Ferrofin/Jellyfin mismatch. The earlier
advice that a dot-sample should be ignored describes the tested release's
behavior, not the documented contract.

The section also covers TV. A separate live fixture contains a series extra and
trailer, plus a season extra and trailer. Jellyfin owns all four under their
series/season. Ferrofin discovers them as episodes, and treats the series'
`extras` folder as a season. This is a confirmed discovery gap.

A source comparison found another gap: Jellyfin's `Video.GetExtraOwnerIds`
collects owners across media versions and `Series.GetExtraOwnerIds` across
grouped series. Ferrofin's existing extras endpoints query the requested owner
alone. The new counts agree with those endpoints; neither path yet aggregates
those other owners. This case was identified in source, not exercised by the
34-layout movie fixture. Music-video libraries and every client-specific theme
playback preference were not validated by this audit.

Follow-up work remains open: correct the plain-space sample discrepancy, port
series/season extras discovery, and share Jellyfin's owner aggregation between
counts and retrieval. Each needs regression cases and a live reference check.
The count fix must not be described as full extras parity.

## Browser validation

Headless Chromium ran the existing Jellyfin web distribution against a local
Ferrofin server. A separate Office Space-style fixture contained a movie,
`Extras/clip.mkv`, a hyphen-sample, a hyphen-trailer, and a dot-sample.

- The movie page displays two Special Features cards and the trailer button.
- Clicking each of the two special features selects its local file and
  produces a browser `playing` event.
- Clicking the trailer button selects the local trailer file and produces a
  browser `playing` event.
- The dot-sample is excluded; movie-detail counts are 2 and 1.
- Repeated library scans preserve counts and owner endpoint results.

The first browser attempt reused a page still in playback and timed out when
returning to details. The completed run reloads between special features and opens a fresh page
before testing the trailer.
This validates playback startup, not a long playback session or seeking.
All tests use disposable media/databases; the user's production database was
not changed or used by these server processes.

## Evidence and commands

- Regression tests: `cargo nextest run -p ferrofin-core -E 'test(extra_counts)'`.
  Both pass. Tests cover every extra discriminant, Unknown, theme exclusions,
  NULL/invalid types, zero counts, repeated owners, independent field selection,
  omitted JSON fields, chunk boundaries, query errors, and indexed access.
- Independent review using `.claude/skills/review-loop/SKILL.md`: no findings.
- Movie audit: `/tmp/ferrofin-extras-doc-audit.py jellyfin` and `ferrofin`.
  Add `--tv` for the series/season cases or `--browser` for the Ferrofin web test.
- Audit artifacts: `/tmp/ferrofin-extras-doc-audit/{jellyfin,ferrofin}.json`,
  `jellyfin-tv.json`, `ferrofin-tv.json`, `browser-results.json`, `web-extras.png`.
- Browser harness: `/tmp/ferrofin-extras-browser.cjs`.

## Workspace validation

- `cargo build --workspace`: passed.
- `cargo fmt --all --check`: passed.
- `cargo clippy --all-targets --all-features -- -D warnings`: passed.
- `cargo nextest run --workspace --no-fail-fast`: 7,371 passed, 5 skipped.
- `cargo test --workspace --doc`: passed.

Two earlier full runs exposed a race in the existing scan-metrics test: the
scan waiter completes before the queue worker drops its in-progress gauge.
The isolated test passed. Commit `81bf649d` adds a bounded wait for the worker's
idle gauge before reading the asserted snapshot; all original assertions remain.
That test-only change passed a separate independent review, and the complete
suite then passed. Neither scanner behavior nor production metrics changed.

Logs: `/tmp/ferrofin-extras-count-{build,fmt,clippy-final,nextest-verified,doc}.log`.
The unavailable compiler cache was disabled with `RUSTC_WRAPPER=`.

Core-only line coverage is **94.15%** (1,904 tests passed). The traits crate and
server composition root are exempt from the line-coverage gate. The report warns that 17 functions have mismatched profile data. This warning
persists after `cargo llvm-cov clean --workspace` and a complete instrumented
rerun, so stale workspace artifacts have not been established as its cause.
The clean run exits successfully and reports 94.15%; retain the warning as a
coverage-tool limitation rather than calling the report warning-free. The
changed DTO and count-service files report 93.58% and 99.47% line coverage.
Dependency crates were excluded so the 80% gate applies to core alone.

Command (with `RUSTC_WRAPPER=` and
`CARGO_LLVM_COV_TARGET_DIR=target/extras-coverage`):

```sh
cargo llvm-cov clean --workspace
cargo llvm-cov nextest -p ferrofin-core --fail-under-lines 80 --summary-only \
  --ignore-filename-regex 'ferrofin-(api|chromaprint|common|db|drawing|extensions|health|hls|keyframes|livetv|mediaencoding|metrics|model|naming|networking|providers|traits|util|wasm)/|/apps/|/usr/|/rustc/|/\.cargo/'
```

Clean coverage log: `/tmp/ferrofin-extras-count-coverage-clean.log`.

## Before/after performance

Three alternating runs per build used identical disposable databases with
200 movies and 400 extras. The baseline is the saved pre-count-fix binary;
the fixed binary includes `c9b9f854`. Both are native debug builds. Each endpoint
has 10 warm-up calls and 100 measured calls. The server is pinned to CPUs 7 and
6 (different physical cores, initially 5.9% and 8.0% busy); other validation jobs
had finished. The final response for each count-bearing endpoint is checked in every run:
baseline zero, fixed one special feature and one trailer per movie.

Values below are median p50 milliseconds over three runs, with their range.

| Request | Before | After |
| --- | ---: | ---: |
| Movie detail | 2.48 [2.24–2.51] | 2.62 [2.52–2.71] |
| 100 movies, count fields | 11.55 [11.18–13.93] | 12.52 [12.30–21.69] |
| 100 movies, no count fields | 11.91 [11.19–19.87] | 11.74 [11.37–13.74] |

The observed median differences are +0.14 ms for a detail request and +0.98 ms
for the 100-item page requesting counts. The indexed batch adds work only when
those fields are requested. Ranges remain wide on this shared host, so these
measurements do not establish production latency or a precise performance
regression. Earlier unpinned runs were noisy and are not used in this table.

Harness: `/tmp/ferrofin-extras-count-perf-pinned.py`.
Results: `/tmp/ferrofin-extras-count-perf-pinned/results.json`.
Log: `/tmp/ferrofin-extras-count-perf-pinned.log`.


## Follow-up

The remaining cases identified here are addressed in the
[extras parity follow-up](extras-parity-follow-up.md), including series/season
extras, grouped owners, music videos, naming exceptions and web theme playback.
