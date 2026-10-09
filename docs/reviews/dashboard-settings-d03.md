# D03: display specials within seasons

The original support classification covered NextUp but missed episode lists.
The reviewed implementation consumes the live saved
`DisplaySpecialsWithinSeasons` value in `/Shows/{series}/Episodes` and ordinary
Season `/Items` browsing, including default UserViews that delegate to a Season.
Numbered requests resolve an actual visible Season; unknown numbers return an
empty list, and a supplied Season ID uses that Season's actual Series.

Positioned specials follow the shared aired-order comparer. Season zero keeps
physical membership and SortName ordering. Full-series selection preserves
source season order and removes repeated IDs at their last season occurrence,
so the same special is counted once. Filtering and adjacency precede count and
paging. Nil relation/adjacency IDs use the source fallback or absent-input
behavior. Domain LocationType follows physical path/source facts rather than
substituting the stored IsVirtualItem bit.

Season `/Items` uses the pinned missing-episode inclusion before the shared
Folder filter/sort tail; `/Shows` retains its different user missing preference.
Both recursion modes, the selected user's parent visibility, explicit-ID
bypass, unplayed/name filters, descending aired order and pages have
regressions. `/Items` uses the Season's own `IsVisible` contract: library grants
alone do not deny a Season browse, while an own-parent tag block returns 401.
`/Shows` resolves the Season through standalone visibility and returns 404 in
both denied cases. The real HTTP fixture authenticates with an admin user
session and selects the restricted user; clearing the selected user's tag block
restores both routes without restarting. API consumers use traits and the pure model comparer; API does not
depend on core. Existing NextUp behavior remains covered.

The real scan exposed a separate missing input: the NFO reader parsed the
three nullable `AirsBeforeSeasonNumber`, `AirsAfterSeasonNumber` and
`AirsBeforeEpisodeNumber` fields but never copied them into provider metadata.
They now reach the existing merge policy. Real scan/refresh regressions cover
zero and negative values, retained stored values when positions are omitted,
clearing with replacement plus removal, and local NFO precedence over remote
TVDB positions. Resolver facts, title and physical Season zero survive.

Source contract: Jellyfin `4910aafa1a`, `Series.GetSeasonEpisodes`,
`Series.GetEpisodes`, `Season.GetItemsInternal`, `TvShowsController.GetEpisodes`,
`AiredEpisodeOrderComparer`, and `TVSeriesManager`. Native Jellyfin 12.1.0 is
separate runtime evidence. The production fixture scans real MKVs/NFO,
requires a known task's new nonempty EndTimeUtc plus Completed status and all
four expected titles, then checks false → true → false membership/order/count.
It includes blocked-tag and selected-user parent controls; raw before results
are observations, while after/reference phases assert their expected results.

[S03, S21, S22, S24 and S25](../../brain/knowledge/JELLYFIN_WEB_DASHBOARD_SETTINGS_REVIEW.md#additional-source-resolution-finding-from-implementation)
remain separate: physical hierarchy, broader comparers, active-DVR user-data
keys, special view producers and other domain child selection are not established
by this setting's episode-list checks.

Required validation includes core/API/model tests and separate coverage >=80%
each, SQL boundary, real HTTP, production/native comparisons, build, final-milestone doctests
and strict workspace/all-targets/all-features Clippy. Frozen provenance and
runner commands are recorded under
`/tmp/ferrofin-d03-independent-review/`. The failed count-type builds and
HTTP assertions are preserved under `/tmp/ferrofin-d03-root-count-types/` and
`/tmp/ferrofin-d03-http-selection-failure/`; corrected source and pinned-source
visibility evidence are retained in the additive D03 freeze.

The final source and closed normal gates are bound in
`/tmp/ferrofin-d03-source26-freeze-v6/final-freeze.json`
(`8af86b4e27fa92477ca735b8ea37302adcdc5a2aa1cfd52c41a488aa0c6c934a`).
All 32 source/fixture files match the reconstructed prefix and all 22
successor patches apply strictly. The released runner-list proof is
`/tmp/ferrofin-d03-count-types-manifest-preparation/final-v6-runner-list-checks.json`.
Fresh full-suite coverage ran without prior seeds: core 2,371/2,371,
API 908/908, and model 980/980 tests passed. The API export reports
53 functions with mismatched data; that diagnostic is retained in the raw
export and coverage JSON rather than suppressed. Core and model exports
have no LLVM diagnostics.

Validation passed:

- fmt: passed.
- core: 161 tests run: 161 passed, 2210 skipped.
- api: 66 tests run: 66 passed, 842 skipped.
- model: 19 tests run: 19 passed, 961 skipped.
- sql-boundary: 1 test run: 1 passed, 0 skipped.
- http: 1 test run: 1 passed, 0 skipped.
- build: passed.
- clippy: passed.

Each changed nonexempt crate passed its own line-coverage gate:

- ferrofin-core: 115,686/121,357 lines (95.33%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: no LLVM diagnostics.
- ferrofin-api: 41,359/47,941 lines (86.27%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: warning: 53 functions have mismatched data.
- ferrofin-model: 8,934/9,901 lines (90.23%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: no LLVM diagnostics.

Evidence: `/tmp/ferrofin-dashboard-d03-checks.json`,
`/tmp/ferrofin-dashboard-d03-coverage.json`, and the passed native before/after/reference JSON records.
Storage recorded at the strict Clippy gate: 21.72 GiB generated target, 534.97 GiB free on the host.
Builds/coverage/native processes were serialized with one compiler job and
incremental/debug data disabled. The source worktree and commits are preserved.


Measured local before/after observations (milliseconds):

| Phase / consumer / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| False / season-number / get_ms | 2.741 | 3.803 | 12.516 |
| False / season-id / get_ms | 2.739 | 3.605 | 16.707 |
| False / specials-season / get_ms | 2.531 | 3.123 | 13.370 |
| False / all / get_ms | 2.904 | 3.287 | 12.591 |
| False / page / get_ms | 2.460 | 2.676 | 12.159 |
| False / unknown-season / get_ms | 2.363 | 2.289 | 9.816 |
| False / nil-adjacency / get_ms | 2.632 | 4.026 | 11.317 |
| True / season-number / get_ms | 2.633 | 3.192 | 11.819 |
| True / season-id / get_ms | 3.005 | 3.158 | 10.122 |
| True / specials-season / get_ms | 2.300 | 2.751 | 12.816 |
| True / all / get_ms | 2.689 | 3.227 | 10.404 |
| True / page / get_ms | 2.275 | 2.947 | 11.160 |
| True / unknown-season / get_ms | 1.781 | 2.122 | 10.592 |
| True / nil-adjacency / get_ms | 2.567 | 3.741 | 15.530 |
| False / season-number / get_ms (occurrence 2) | 3.440 | 3.522 | 10.900 |
| False / season-id / get_ms (occurrence 2) | 3.216 | 3.499 | 10.071 |
| False / specials-season / get_ms (occurrence 2) | 2.668 | 2.784 | 11.085 |
| False / all / get_ms (occurrence 2) | 2.953 | 3.642 | 9.936 |
| False / page / get_ms (occurrence 2) | 2.268 | 2.706 | 11.289 |
| False / unknown-season / get_ms (occurrence 2) | 1.836 | 2.172 | 9.615 |
| False / nil-adjacency / get_ms (occurrence 2) | 2.602 | 3.166 | 11.736 |

Paired values show median / p95 where the native fixture records repeated
HTTP reads; single values are the actual recorded request or completion time.
Before uses the preserved parent binary; after uses the verified current server. Each
run creates the same isolated data and exercises the same setting transitions.
Unavailable means that consumer was absent or did not complete in that run.
These are local observations on a shared host, not publishable benchmark claims.
The preferred Docker benchmark was unavailable; no quiet-host result is claimed.
The runtime reference is separate from the pinned source oracle.
