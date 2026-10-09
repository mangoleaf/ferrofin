# L03: disabled libraries stay out of listings and counts

Saving `LibraryOptions.Enabled=false` now hides that library from the user root,
unscoped item queries and their counts immediately, including for administrators.
Direct item visibility and home views already enforced the option; the repository
paths were missing it. They now read the same saved JSON/XML options without
creating files or mutating library rows. Re-enabling takes effect on the next
request, without a scan or restart.

Source: Jellyfin `4910aafa1a`, `CollectionFolder.IsVisible`,
`Folder.AddChildrenFromCollection` and `LibraryManager.AddUserToQuery`.
The original audit incorrectly suggested disabling should stop scans: upstream
uses this option for visibility. Explicit-scope browse and userless internal
root reads retain the upstream exceptions; per-user folder restrictions still
apply independently.

All **42 native observations match Jellyfin 12.1.0** across enabled/disabled/
re-enabled states and administrator/ordinary users. The six previously different
responses—root listings, movie lists and counts for both users—now match. Direct
library/movie lookup, home views and explicit library browse remain consistent
with the reference. Evidence: `/tmp/ferrofin-dashboard-l03-ferrofin-lwjz01x2/results.json`.

Validation passes: **223 core tests, 28 repository tests, ten real-manager HTTP
tests**, SQL boundary, formatting, strict workspace Clippy and server build.
Regressions verify live JSON toggles across pages, counts and visible-library
IDs, adopted XML reads without writeback, root filtering and scope exceptions.

The full core coverage run passes **2,160 tests** and retains the same four host
watcher failures. The separate 80% threshold passes; restricting LLVM to the 22
current test binaries reports **94.99%** (102,012 / 107,388 lines), with 19
mismatched-function warnings. The workspace watcher limitation documented in L01
remains open; no host limits were changed.

Fifty warm ordinary-user movie queries after ten warmups measured median
**9.732 → 8.036 ms**, p95 **19.086 → 23.472 ms** against the immediate parent and
this build. Shared-host debug timings show substantial variance and are not a
production performance claim. Both matrices completed without request failures.
Evidence: `/tmp/ferrofin-dashboard-l03-checks.json`,
`/tmp/ferrofin-dashboard-l03-coverage.json`,
`/tmp/ferrofin-dashboard-l03-exact-coverage-summary.json`, and the parent fixture
`/tmp/ferrofin-dashboard-l03-ferrofin-qibe2ofs/results.json`.
