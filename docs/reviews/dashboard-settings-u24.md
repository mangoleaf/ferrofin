# U24: allowed and blocked tags

Saved tag restrictions now reach browse, explicit-ID queries, Latest, facets,
counts and descendant/played-state queries through the shared parental predicate.
The direct visibility evaluator already enforced them; lists previously exposed
items and counts that direct access rejected.

The query checks the item's own tags, SeriesId, AncestorIds and TopParentId.
Blocked tags take precedence over allowed tags; matching removes diacritics and
normalizes case. People retain upstream's allowed-tag exception, while blocked
tags still apply. A by-name row must also have accessible related media. No new
index, schema migration or data rewrite is needed. Each tag set uses four indexed
membership checks, matching the pinned source's query shape.

Root children retain the allowed-tag exception. Home views now share the batched
item visibility evaluator, so a library with a blocked tag disappears even when
`includeHidden=true`; its media also disappears through inherited tags. Hiding a
home view through display preferences remains a separate operation.

Compared against Jellyfin `4910aafa1a`: `InternalItemsQuery.SetUser`,
`BaseItemRepository.ApplyParentalRestrictions`, `ApplyItemByNameAccessFiltering`,
`BaseItem.IsVisibleViaTags`, `GetAncestorIds` and `UserViewManager.GetUserViews`.
Native Jellyfin 12.1.0 is supplementary runtime evidence, not the source pin.

Validation: 433 focused core tests, 28 repository tests and nine real-manager
HTTP tests pass. The regressions cover each inheritance source, Unicode,
block-list precedence, empty/blank policies, explicit IDs, Person exceptions,
live saves, counts and root views. An EXPLAIN QUERY PLAN regression verifies
indexed tag-value/map and ancestor searches. SQL boundary, formatting, strict
workspace Clippy and server build are checked alongside the change.

All **60 native HTTP observations match Jellyfin** after materializing the
shared genre before the matrix. UUIDs are normalized, and the reference's
pre-existing extra Playlists root is excluded from the name comparison; neither
normalization alters tag-policy outcomes. Twenty-one observations change from
the baseline. The fixture repeats empty, blocked, allowed, conflicting and reset
policies, then tags a library and verifies inheritance and root visibility.

Native debug-build timings, 50 requests after ten warmups:

| Path | Before median / p95 ms | After median / p95 ms |
|---|---:|---:|
| Movie browse | 2.066 / 2.697 | 2.355 / 5.045 |
| Genres | 1.589 / 2.343 | 2.084 / 4.379 |
| Genre detail | 1.691 / 2.244 | 1.951 / 4.477 |
| Library views | 2.033 / 2.769 | 4.030 / 5.134 |

This is a two-movie fixture on a shared host with other builds/probes running,
not a production performance claim. Corrected browse/count paths return one
movie rather than two; view filtering adds batched policy and hierarchy reads.
Docker benchmarking remains unavailable in this environment.

Evidence: `/tmp/ferrofin-dashboard-u24-checks.json`,
`/tmp/ferrofin-dashboard-u24-comparison.json` and the three fixture result paths
listed in that comparison. A clean core coverage run passes **2,144 tests** and
reports **94.59% line coverage**, with 18 LLVM mismatched-function warnings still
present. This passes the gate with the measurement limitation retained. See
`/tmp/ferrofin-dashboard-u24-coverage.json` and its referenced log.
