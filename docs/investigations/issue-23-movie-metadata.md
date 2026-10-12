# Issue #23: movie metadata after Jellyfin adoption

Investigated 2026-09-28. [Issue and reporter follow-up](https://github.com/mangoleaf/ferrofin/issues/23).
Branch: `fix/movie-metadata-23`, based on `feat/scan-change-detection` at `71311ef2`.
Compared with main at `1271ecfe`.

## Follow-up: shared credentials

The provider-key audit found a missed Jellyfin behavior: OMDb ships a shared API
key. Ferrofin previously required an operator key, so selecting OMDb alone could
silently produce no requests. This branch now uses Jellyfin's shared key by default
and retains `FERROFIN_OMDB_KEY` / `omdb_api_key` as optional overrides. The real-server
HTTP test verifies OMDb-only lookup with no operator key and the library opt-out.
See the [complete provider audit](shared-provider-keys.md), which also found and
added the missing OpenSubtitles application key.

This is a confirmed configuration mismatch, but we still cannot establish whether
the reporter had supplied an OMDb key or which provider emitted their errors.

## Finding

The strongest explanation for the lost movie metadata is the destructive rescan
behavior already addressed by [PR #21](https://github.com/mangoleaf/ferrofin/pull/21).
Main reconstructs each movie from its path and saves that reconstructed row even
when remote metadata lookup fails. Consequently, a provider outage can erase
metadata that was already present in an adopted database. This is a code-confirmed
failure path; the reporter's exact trigger remains unconfirmed without logs or a
copy of the affected data.

The report is more specific than "movies never populate": the reporter had working
Jellyfin metadata before migration, lost it after deploying Ferrofin, saw provider
backoff during refreshes, and restored the movies through Identify. They did not
supply the actual errors, image digest, filenames, or provider settings.

## Code trace

- On main, `crates/ferrofin-core/src/library_scan.rs`, `scan_and_persist`, starts
  each item with `item.entity.clone()`, runs providers, then calls
  `save_scanned_items`. There is no stored-row metadata merge. Movie TMDB gates
  inspect the reconstructed entity, so previously populated movies can trigger
  requests again on every scan. A failed lookup returns no metadata and the
  reconstructed empty fields reach persistence.
- On the investigation base, `LibraryScanner::scan_item` merges results through
  `saved_row` and `MergeMode::of`. Stored values survive failed requests, and
  `DateLastRefreshed` advances only after a successful pass. The refresh plan
  skips work for unchanged, successfully refreshed items.
- Automatic movie lookup is implemented: `fetch_tmdb_metadata` resolves known TMDB
  IDs, then external IDs, then title/year; `fetch_omdb_metadata` uses IMDb IDs or
  exact-title lookup. Identify pins the chosen result and runs a full refresh.
  Its success does not prove an automatic-lookup outage has a different cause:
  it also changes the timing, scope, and sometimes the lookup inputs.
- `apps/ferrofin-server/src/state.rs` wires metadata independently of successful
  media probing. A missing ffprobe can fail the probe and prevent a successful
  refresh stamp, but does not itself prevent TMDB from supplying movie text.
- Database adoption in `crates/ferrofin-db/src/database.rs` baselines an existing
  supported schema; it does not intentionally clear movie text. This review did
  not reproduce the reporter's complete migration.

## Provider errors and migration paths

Do not attribute the reported backoff to provider policy without its status/error.
`crates/ferrofin-providers/src/rate_limit.rs` backs off on 408, 429, 500, 502, 503,
504 and transport failures. It shares cooldown state across callers and can skip
later calls without sending HTTP when a cooldown exceeds its wait budget. One
rejected request can therefore affect a whole refresh, even for a small library.
The number of movies is also not the request count: artwork and people have their
own requests, and repeated scans add more.

Before this follow-up, OMDb had its own limiter but required an operator API key.
Selecting its checkbox alone did not enable it. The shared default now fixes that.
Fanart supplies images, not movie synopses. These are checks for follow-up, not
confirmed misconfiguration in this report.

Missing copied images or changed container mount paths are a separate plausible
contributor. `VirtualPathExpander` expands Jellyfin's `%MetadataPath%` and
`%AppDataPath%` tokens, but cannot recover files that were not copied or arbitrary
old absolute paths. [PR #14](https://github.com/mangoleaf/ferrofin/pull/14) fixes the
native migration instructions/script; it does not establish what happened in this
Docker deployment. An image-path problem alone does not explain erased synopses.

## Explicit image replacement during an outage

The HTTP reproduction found a second way to end up without artwork, still present
on PR #21: requesting both `ReplaceAllMetadata=true` and `ReplaceAllImages=true`
deletes cached images before contacting the provider. With a provider outage,
there is no replacement to display. Movie text and cast still survive on PR #21.

This matches the checked-in Jellyfin source at
`Jellyfin.Api/Controllers/ItemRefreshController.cs` (`RemoveOldMetadata =
replaceAllMetadata`) and `MediaBrowser.Providers/Manager/MetadataService.cs`
(`RemoveOldMetadata && ReplaceAllImages` calls `ImageProvider.RemoveImages`
before fetching). Ferrofin follows it in `LibraryScanner::collect_artwork`.
It is not evidence that the reporter selected those flags during the original
migration, but it matters to the proposed diagnostic refresh with image replacement.

A full metadata refresh with `ReplaceAllImages=false` preserved the existing poster
in the reproduction. Use that while diagnosing provider failures. Preserving images
even under explicit removal would be a separate behavior change from Jellyfin.

## Regression coverage

Extended `apps/ferrofin-server/tests/scan_change_detection.rs` with an issue-specific
scenario after the existing HTTP matrix. It runs the real server against mock
providers and its SQLite database:

1. Start with movie metadata, cast and artwork populated by the existing matrix.
2. Remove the movie's field lock, so locks cannot conceal destructive behavior.
3. Make ffprobe unavailable and add a fresh movie through the library webhook.
   Verify that automatic TMDB lookup still populates its overview and provider ID.
4. Have TMDB return HTTP 429 with a long Retry-After; request a movie-library full
   metadata replacement with image replacement disabled.
5. Check that the shared cooldown permits only one outbound TMDB request; stored
   text, ratings, dates, provider IDs, trailers, cast, image tags and runtime survive;
   the failed refresh does not advance DateLastRefreshed; the poster still serves
   successfully over HTTP.
6. Repeat with both replacement flags enabled. Verify no further TMDB request gets
   past the shared cooldown, cached artwork is removed, and text/cast remain.

This exercises refresh failure after metadata exists. It does not simulate adoption
or establish why the reporter's real provider requests failed. The initial investigation proposed no production change beyond PR #21. The
subsequent credential audit found and fixed the missing shared OMDb key.

## Initial investigation validation results

All checks passed:

```sh
cargo test -p ferrofin-server --test scan_change_detection -- --nocapture
cargo clippy -p ferrofin-server --test scan_change_detection -- -D warnings
cargo fmt --all --check
git diff --check
```

The full HTTP matrix finished in 14.91 seconds. Its added scenarios observed:

| Scenario | TMDB HTTP requests | Result |
|---|---:|---|
| New movie, ffprobe missing | 3 | Overview and TMDB ID populated |
| Replace metadata, TMDB 429 | 1 | Stored metadata and poster preserved |
| Also replace images, cooldown active | 0 | Cached poster removed; text and cast preserved |

The provider metric includes logical calls rejected by the shared gate; those
are not extra HTTP requests. No production code changed, so no performance
benchmark or full workspace test run was needed for this investigation.

## Recommended follow-up

Test the PR #21 build first. After providers recover, refresh the affected library
once if metadata has already been lost, leaving image replacement disabled. A successful Identify should also survive
subsequent scans on that build.

If the issue persists, request the image digest, one representative movie path,
movie metadata/image fetcher settings, and debug logs spanning a refresh. The useful
lines include `Metadata provider ...`, their `provider`, `status`, `retry_after`
and transport error fields, and `item refresh decided` for the affected movie.
Check the provider IDs and artwork paths for that item before and after the scan.
For older builds, establish whether OMDb had a key configured without asking for
the key itself. On this branch, leaving the override unset uses the shared key.
