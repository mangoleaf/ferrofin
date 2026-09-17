# Movie browse: paginate before loading full rows

## Change

The movie grid previously sorted complete `BaseItems` rows, including metadata,
before discarding rows outside the requested page. Deep pages were especially
expensive. The query now materializes the ordered page's IDs and sort keys in
`movie_page`, then fetches only that page's complete rows by primary key. A
`CROSS JOIN` keeps the page as the outer loop; the final sort handles only the
returned page.

This applies to grouped movie queries with a positive limit and ascending
`SortName`, optionally followed by ascending `ProductionYear`. The existing
filter, grouping, representative selection and paging builders are reused.
The existing `Name` tiebreaker stays between `SortName` and `ProductionYear`.
Search and other ordering forms retain their existing query paths. No schema
or index changes are needed; the total-count query is unchanged.

## Correctness and checks

- Full HTTP JSON responses match between the baseline and optimized release
  binaries for all 31 library pages: 3,001 movies, identical rows, ordering,
  fields and totals.
- Repository regression tests compare complete rows against the previous SQL
  projection. They cover tied names and years, NULL sort keys, both supported
  orderings, alternate-version grouping, a filter selecting only an alternate,
  library scoping, empty/short/full pages, offsets past the end, and zero and
  negative limits.
- An `EXPLAIN QUERY PLAN` regression requires materialization of the page and
  primary-key lookups driven by the page, preventing full-library hydration.
- Fallback checks cover search, other item types, ungrouped queries, unbounded
  queries, other sort orders and aggregate/ID-only projections.
- Workspace nextest: **6,465 passed**, 5 skipped. The final repository test
  rerun after fixture refinements: **24 passed**.
- Workspace formatting, Clippy with all targets/features and warnings denied,
  and workspace doctests passed.
- `cargo llvm-cov nextest -p ferrofin-core --fail-under-lines 80 --summary-only`:
  **1,451 tests passed; reported line coverage 82.48%**.

## HTTP benchmark method

Measured locally on 2026-09-16 (US/Mountain), against base commit `0e2d24ee`.
Both binaries were built with the pinned Rust 1.98.1 toolchain and
`cargo build --release -p ferrofin-server --offline`. The baseline binary was
built before the production change; its SQL does not contain `movie_page`.

The existing `bench/screens.js` exercised all six screens at five screens per
second, using a 30-second discarded warmup and a 120-second measured window.
Each server ran alone on cores 8–15, with k6 on 16–19. Both used freshly restored
copies of the same saved `v1.0.0` benchmark database and a disposable copy of its
media, with paths rewritten identically for local access and extensions disabled.
Startup tasks drained, followed by a 30-second settle. Compilation and tests
finished before the final timing runs. An earlier baseline taken while checks
were running was discarded.

Docker access was unavailable, so this uses local processes without the normal
container memory limit. These are one before/after pair for this change, not a
replacement for the published three-run Jellyfin comparison.

Local artifacts (gitignored) are in `bench/runs/movie-browse-local/` in this
worktree: `method.json` records binary hashes, `run-local.py` records the exact
runner, and each server's directory contains k6 results, HTTP page snapshots,
and logs. Validation/build logs are alongside them.

## Results

Milliseconds, p50 / p95:

| Measurement | Baseline | Optimized |
|---|---:|---:|
| Movie screen | 24 / 31.05 | **13 / 17** |
| Movie list endpoint | 22.26 / 29.40 | **11.19 / 15.14** |
| Home screen | 14 / 20 | 14 / 21 |
| Detail screen | 6 / 8 | 6 / 9 |
| Series screen | 5 / 7 | 5.5 / 8.05 |
| Search screen | 29 / 40.45 | 26 / 35.10 |
| Playback screen | 9.5 / 11.25 | 10 / 17.05 |

The movie screen's median latency fell **46%** (1.85x faster); the movie list
endpoint's median fell **50%** (1.99x faster). Its p99 fell from 31.41 to 16.61 ms.
Each run measured 120 movie screens. All endpoints had a 100% success rate and
neither run dropped iterations. The complete mix had 601 versus 600 iterations
(an arrival at the window boundary), with 8,785 versus 8,765 requests.

Other screen timings, especially playback's tail, varied between this single
pair; this does not establish improvements or regressions on those paths. Three
full runs under the published container conditions are still needed before
updating headline Jellyfin comparisons. The saved `comparison.json` contains
the full endpoint and screen results plus the HTTP page-equality verdict.
