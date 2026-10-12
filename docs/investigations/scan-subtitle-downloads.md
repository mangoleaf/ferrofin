# Subtitle downloads during scans

Implemented on `fix/movie-metadata-23`, continuing the movie metadata investigation.

## Behavior

A successful movie/episode probe during a Default or FullRefresh pass checks the
library's `SubtitleDownloadLanguages`. This follows Jellyfin's
[`FFProbeVideoInfo.AddExternalSubtitlesAsync`](https://github.com/jellyfin/jellyfin/blob/master/MediaBrowser.Providers/MediaInfo/FFProbeVideoInfo.cs).
Unchanged items, failed probes, image-only refreshes and validation-only refreshes
make no subtitle requests. A full metadata refresh can fill missing subtitles for
previously scanned items after the operator enables download languages. The daily
task remains the retry path for missing results and provider failures.

The scan and scheduled task share one downloader and the
[`SubtitleDownloader`](https://github.com/jellyfin/jellyfin/blob/master/MediaBrowser.Providers/MediaInfo/SubtitleDownloader.cs)
checks: existing external subtitles or embedded text satisfy a language; the
embedded-subtitle option also allows embedded image subtitles to satisfy it. Audio
matching uses default tracks, or the first audio track when no default is marked.
Duplicate language entries are coalesced without case sensitivity. The library's
disabled fetchers, provider order and perfect-match requirement are respected.

The OpenSubtitles provider now computes its file hash (size plus the first/last
64 KiB), requests hash matches when required, and filters nonmatching candidates.
This closes the old provider's ignored perfect-match setting. Small files without
a usable hash cannot satisfy perfect-match mode.

## Scan safety

Subtitle attachment follows item, provider-ID and probe-stream persistence so the
new item can be resolved and a later probe save cannot erase its downloaded streams.
A shared gate prevents overlap with the scheduled task. A probe preserves subtitle
rows attached after its external-file snapshot when their files still exist.
Deleted sidecars are not preserved. Libraries without download languages take the
existing stream-save path without extra database queries.

Cancellation interrupts search/download waits and gate acquisition. Once subtitle
bytes are available, the local file and stream-row save finishes together. Provider
errors are logged and do not fail the media scan or clear its metadata refresh stamp.
The manager reference is weak to avoid a scanner/library/manager ownership cycle.

## Validation

The real-server HTTP scan matrix covers initial downloads, pre-existing sidecars,
stream visibility, repeated normal and full refreshes, removed sidecar recovery,
disabled providers, provider outages and a quiet unchanged scan after an outage.
Unit tests cover embedded text/image subtitles, default audio, duplicate/concurrent
downloads, late subtitle persistence, cancellation during both network phases,
provider order, perfect-match filtering and the OpenSubtitles hash algorithm.

Completed commands (all Rust checks use `RUSTC_WRAPPER=` and this worktree's
isolated target directory):

- `cargo nextest run -p ferrofin-core -p ferrofin-providers --no-fail-fast`:
  2,405 passed, 4 skipped.
- `cargo nextest run -p ferrofin-traits`: 74 passed.
- `cargo test -p ferrofin-server --test scan_change_detection -- --nocapture`:
  the complete HTTP scan matrix passed, including the original movie metadata
  outage cases and the new subtitle cases.
- `cargo clippy -p ferrofin-core -p ferrofin-providers -p ferrofin-server --all-targets --all-features -- -D warnings`: passed.
- `cargo fmt --all -- --check` and `git diff --check`: passed.
- `cargo llvm-cov nextest -p ferrofin-core --fail-under-lines 80 --summary-only`:
  passed; core source files have 93.95% line coverage (the report including
  exercised workspace dependencies totals 82.56%). The new downloader has 96.88%.
- Provider coverage: 579 passed, 4 skipped. The provider-only report passes
  the 80% gate at 92.36% line coverage. The initial report included workspace
  dependencies that provider tests do not exercise (70.30% combined), so the
  per-crate gate was checked with:

  ```sh
  RUSTC_WRAPPER= cargo llvm-cov report -p ferrofin-providers --summary-only \
    --ignore-filename-regex 'ferrofin-(common|db|model|naming|traits|util)/' \
    --fail-under-lines 80
  ```

The full workspace suite was not run. All service calls in these subtitle tests
use local mocks; no real account quota was consumed.

## Scan timing

Measured the existing `apps/ferrofin-server/tests/scan_bench.rs` harness before
this subtitle change and after it, using debug test binaries from the same
worktree. Four runs per version, alternating execution order, after builds and
coverage reporting finished. Each run creates 300 movies and 10 series with
10 episodes each (420 scanned items including seasons/series).

| Median wall time | Before | After |
|---|---:|---:|
| First scan | 1.1245 s | 1.1100 s |
| Rescan | 0.7005 s | 0.6740 s |

No slowdown was observed in this local fixture. This harness has no ffprobe or
remote providers enabled, so these numbers measure the existing path without
subtitle downloads; they do not predict service latency or large-library timings.
The HTTP matrix separately proves unchanged scans avoid probes, subtitle requests
and stream writes when subtitle downloads are configured.

Reproduction settings:

```sh
FERROFIN_SCAN_BENCH=1 FERROFIN_SCAN_BENCH_MOVIES=300 \
FERROFIN_SCAN_BENCH_SERIES=10 FERROFIN_SCAN_BENCH_EPISODES=10 \
RUSTC_WRAPPER= cargo test -p ferrofin-server --test scan_bench -- --nocapture
```

The before/after binaries were copied to `/tmp/ferrofin-subtitle-scan-bench-before`
and `/tmp/ferrofin-subtitle-scan-bench-after`. Raw measurements are in
`/tmp/ferrofin-subtitle-bench-final.log` and `.json` for this session.
