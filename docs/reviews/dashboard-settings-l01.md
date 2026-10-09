# L01: library management and media paths

Library creation and path updates now match the dashboard contract. The physical
folder prerequisite preserves media when libraries share a location.

Source: Jellyfin `4910aafa1a`, `LibraryStructureController`,
`LibraryManager.AddVirtualFolder`, `AddMediaPath`, `UpdateMediaPath`,
`SyncLibraryOptionsToLocations`, `AggregateFolder` and `CollectionFolder`.
The approved D1 physical-folder work was ported from `59aaf532`; unrelated
item-file-deletion commits were not included. U13 remains open.

Creation rejects leading/trailing whitespace and embedded newlines. Both library
creation and AddMediaPath require directories, with upstream's respective 400 and
404 responses. UpdateMediaPath synchronizes existing shortcut locations into the
options instead of adding its request path as a disconnected option entry.
Duplicate names, collection type, locations and removal retain their contracts.

Scanned media now hangs from a physical location Folder below AggregateFolder.
The CollectionFolder records its physical locations and folder IDs; readers,
notifications, provider selection and encoding/library lookups follow that
hierarchy. Shared roots retain all referring library ancestors, and a first
path-scoped refresh provisions its physical parent. Removing one library leaves
another library's media IDs and watched state intact.

A library can be removed before an older database has had a new scan. Before
deleting its CollectionFolder, a transaction moves legacy children under their
physical location, retaining media IDs, metadata and user data. An unresolvable
legacy path rolls back and fails the removal rather than deleting media. The
same protection precedes a rename and orphan-library cleanup; L02 separately
addresses the rename controller's unconditional refresh.

Removing a library with refresh disabled preserves physical rows until a scan,
as upstream does. A successful full scan prunes roots no library references,
including after the last library is removed; an unrelated empty library does not
prevent cleanup. This reads stored parent links: normal browse queries project
AggregateFolder as user views and cannot be used to enumerate its physical
children. Source files are never deleted by these structural controls.

The 14 basic native observations match Jellyfin 12.1.0 exactly; six recorded
steps change from the parent build. Fresh and upgraded shared-directory probes
retain HTTP 200 and Played=true immediately after removal and after rescan;
the parent returned 404 until rescanning. The upgrade starts with the U25 binary,
restarts the same fixture on this build and removes a library without a scan.
Evidence: `/tmp/ferrofin-dashboard-l01-ferrofin-3nirblr0/results.json`,
`/tmp/ferrofin-dashboard-l01-shared-ferrofin-z6bd9gyh/results.json` and
`/tmp/ferrofin-dashboard-l01-shared-ferrofin-7vbe81e1/results.json`.

Fifty warm GET /Library/VirtualFolders requests after ten warmups measured median
**0.950 → 0.980 ms**, p95 **1.311 → 1.316 ms**. These are debug binaries on a
shared host with concurrent checks, not production performance claims.

All **16** extended native observations now match the reference, including
immediate preservation and removal on the next scan after deleting the last
library. Evidence: `/tmp/ferrofin-dashboard-l01-ferrofin-_no9la8a/results.json`.
All **28 adoption stages pass** across the seven supported source/upgrade paths:
initial adoption, restart, scan and repeated scan preserve the fixture metadata,
user data and views. Adoption is one-way; returning to Jellyfin requires restoring
the preserved original backup. These checks use disposable copies.

The full workspace run passes **7,880 tests**, fails seven watcher-dependent
tests and skips five; all three doctests pass. A serial recheck retains those
seven failures (four core watch tests, two encoding file-notification tests,
one scan-change integration test). Core watch creation reports the host inotify
limit, and the scan test explicitly reports that its required watcher could not
start. The earlier full instrumented core run passes all 2,161 tests, including
its watchers. **The workspace gate is not green.** Host settings and unrelated
processes were not changed. Sixteen final physical-location/adoption regression
tests, strict workspace Clippy and the rebuilt native server checks pass.

The more general nested-location hierarchy remains an explicit open finding,
S03: the existing planner flattens intermediate directories. Exact shared roots
are covered here; nested root/subfolder resolution needs its own implementation
and reference matrix. This change does not certify that separate case.

Separate line-coverage thresholds pass: **core 94.98%**, **model 88.73%**,
**API 87.52%**, **providers 93.15%**, **extensions 94.83%**. The final core run
passes 2,158 tests and retains four host watcher failures; its current-binary coverage
report passes the 80% threshold. The other crate runs pass all their tests
(960 model, 892 API, 754 providers with four skipped, 96 extensions). LLVM
mismatched-function warnings remain a measurement limitation; the initial five
reports log 19, 39, 83, 70 and 39 respectively. The ordinary final core report
also picked up stale instrumented binaries and logged 132 warnings. Restricting
LLVM to the 22 current binaries listed by nextest reports 101,858 / 107,241 lines
covered (94.98%), with 19 warnings. This is the core figure recorded above.
Commands and results are in `/tmp/ferrofin-dashboard-l01-{coverage,final-core-coverage}.json`
and `/tmp/ferrofin-dashboard-l01-exact-coverage-summary.json`.
The SQL-boundary gate, formatting and strict workspace Clippy also pass.

No active user library, configuration, database or media file was used for these
fixtures. The implementation is complete for the controls covered here; the
host-dependent workspace failures and S03 remain explicitly open.
