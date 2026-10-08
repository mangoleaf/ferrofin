# L02: rename honors the refresh setting

Renaming with `refreshLibrary=false` (or omitted) no longer queues a scan. The
renamed collection folder is still materialized immediately, so its physical
locations can be browsed. `refreshLibrary=true` retains the scoped refresh.
The L01 hierarchy and legacy-conversion prerequisite preserve media identities,
metadata and user data across the collection-folder ID change.

Source: Jellyfin `4910aafa1a`,
`LibraryStructureController.RenameVirtualFolder`. The prior controller explicitly
ignored the flag to compensate for cascading deletion of media rows. That
compensation is no longer needed after L01.

The API regression uses a library double that panics if a scan is queued. Both
omitted and false flags rename successfully without a scan. All **893 API tests**,
formatting, strict workspace Clippy and server build pass. The separate API
coverage gate reports **87.55%**, with all 893 tests passing and 93 LLVM
mismatched-function warnings retained as a measurement limitation.

Native fixtures edit a movie's title/overview, mark it played, then add an
unscanned second movie before renaming. Before this change, the false flag
indexes that new movie. Afterwards it remains unindexed, matching native
Jellyfin 12.1; an explicit refresh then finds it. An explicit true flag still
indexes it during rename. Existing media retains HTTP 200, its edited metadata
and Played=true in fresh and upgraded databases. The upgrade starts with U25,
restarts on this build and renames without rescanning first.

The reference also resets unlocked title/overview during the later explicit
scan, whereas Ferrofin retains them. That separate metadata-refresh observation
is recorded as S04 for pinned-source verification; it is not counted as rename
parity or silently accepted as a compatibility exception.

Thirty warm rename requests after ten warmups measured median
**7.973 → 7.762 ms**, p95 **32.489 → 26.308 ms**, with 250 ms between requests.
Both builds use disposable two-movie libraries; these shared-host debug timings
are not a production performance claim. No requests failed.

Evidence: `/tmp/ferrofin-dashboard-l02-checks.json`,
`/tmp/ferrofin-dashboard-l02-coverage.json`, native results under
`/tmp/ferrofin-dashboard-l02-ferrofin-{1lua1urb,nz8qjr1o,xy7d6rvu}/results.json`,
and benchmark results under `...-{5tzqp7wk,gc7eplak}/results.json`.
The L01 workspace watcher limitations remain open; this change adds no watcher
or scan dependency to rename with refresh disabled.
