# U22: parental ratings and subratings

The maximum rating now uses all 45 upstream rating resources (44 country
tables; GB/UK share a country key), rather than the US table alone. The server's
default country is read live. Explicit item/library countries, country prefixes,
US fallback, custom ratings, inherited ratings and unrated values retain the
upstream precedence. Existing 12.1 parsing behavior is preserved, including
whole-value matching before slash-separated fallback.

Metadata edits and scans now persist the score used by the shared visibility
resolver. Clearing a rating clears its old numeric value. An idempotent boot
repair recalculates existing rows under the complete catalog, including databases
that already ran the older US-only repair. This changes no schema.

The query audit also found and fixed these bypasses:

- A rated collection/playlist uses its own rating. Only an unrated container
  consults linked children; the former conjunction incorrectly rejected rated
  containers because of an adult child.
- Genre/artist/studio aggregates and by-name search rows now check the ratings
  of related media. Counts on genre, person and year pages apply the same limit
  before aggregation.
- Direct-child, linked-child, descendant and whole-server played counts apply
  the limit. A hidden episode's progress cannot make its series resumable.

Source: Jellyfin `4910aafa1a`, `LocalizationManager.LoadAll/GetRatingScore`,
`BaseItem.GetParentalRatingScore/OnMetadataChanged`,
`BaseItemRepository.QueryBuilding.ApplyParentalRestrictions`,
`BuildMaxParentalRatingFilter`, `ApplyItemByNameAccessFiltering`, and
`ItemCountService`. Rating resources are copied verbatim; 17 upstream rating
cases are transliterated into tests. U23 and U24 separately cover unrated kinds
and tags; their remaining query gaps are not closed by this change.

Validation: 228 focused core tests, 28 repository integration tests, seven
real-manager HTTP tests, SQL boundary, formatting, strict workspace Clippy and
server build pass. Native HTTP checks cover five country changes without a
restart, score/subscore boundaries, direct access, metadata edits, facets and
counts: 80 recorded observations across the two fixtures. The count baseline
exposed all genres and reported three movies even when the user could see none;
the corrected build reports only accessible media.

The boot-repair adoption matrix passes all 28 stages: adoption, restart, scan
and unchanged second scan for 10.11.8–10.11.11, 12.0, and both supported 12.1
upgrade routes. Fixtures are disposable copies. Adoption remains one-way;
returning to Jellyfin requires restoring its backup. This run also exposed the
owner-scoping regression fixed separately in `304683f3` (U19).

Native debug-build timings, 50 requests after ten warmups:

| Path | Before median / p95 ms | After median / p95 ms |
|---|---:|---:|
| Rating choices | 0.428 / 0.559 | 0.451 / 0.956 |
| Movie detail, catalog change | 2.369 / 2.757 | 2.712 / 4.110 |
| Movie browse, catalog change | 1.635 / 1.867 | 1.703 / 2.251 |
| Genres, count/facet fix | 1.918 / 2.133 | 1.820 / 2.472 |
| Genre detail, count/facet fix | 1.795 / 2.012 | 1.521 / 1.900 |
| Person detail, count/facet fix | 1.818 / 2.691 | 1.601 / 2.593 |
| Movie browse, count/facet fix | 1.815 / 2.758 | 1.726 / 2.523 |

These are small-fixture shared-host observations, not production performance
claims. Corrected facets return fewer rows, so their before/after work differs.
The initial three-movie scan was observed at 0.409 → 0.413 seconds with 0.2-second
polling, a coarse smoke check rather than a throughput benchmark.

Evidence: `/tmp/ferrofin-dashboard-u22-{before,after}-results.json`,
`/tmp/ferrofin-dashboard-u22-counts-{before,after}-results.json`,
`/tmp/ferrofin-dashboard-u22-query-checks.json`, and
`/tmp/ferrofin-dashboard-u22-adoption-fixed.log`.

The final workspace checkpoint passes **7,842 tests (five skipped) and three
doctests**. Earlier runs had host inotify `ENOSPC` failures: four core watcher
cases and one watcher-dependent server case. The host condition no longer
reproduces in this checkpoint, and the instrumented watcher subset also passes
on recheck. No host limits or unrelated processes were changed.

The per-crate cargo coverage threshold passes at 80.85%, but that aggregate
includes stale executables and warns about 144 mismatched functions. Restricting
the report to the exact 21 instrumented executables listed by this build's
nextest metadata reports **94.96% core line coverage** (100,718 / 106,069 lines in
123 core source/test files) and retains 18 mismatch warnings. Those profile
warnings remain a validation limitation; both percentage thresholds pass. The
current-binary report and list are
`/tmp/ferrofin-dashboard-u22-exact-coverage-summary.json` and
`/tmp/ferrofin-dashboard-u22-coverage-binaries.json`. The instrumented full core
run passed 2,120 of 2,123 tests before the host condition cleared; its three
watcher failures subsequently pass. Server wiring is exempt from the separate
coverage gate, and its integration tests pass.

Workspace and watcher records:
`/tmp/ferrofin-dashboard-u22-workspace-checks.json` and
`/tmp/ferrofin-dashboard-u22-final-watcher-recheck.log`.
