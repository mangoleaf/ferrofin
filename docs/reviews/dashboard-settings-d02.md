# D02: group movies and shows into collections

The two live global grouping switches now control the Folder consumers that
invoke automatic collection substitution. Movies and series can be enabled
independently; request overrides retain the pinned allow-list and parent-kind
exceptions. Disabled plain Folder browsing still applies the source filtering
and paging path. Series, Season and Channel overrides retain their own dispatch.

Recursive repository selection substitutes collections before sorting, adjacency,
counts and paging. Plain browsing loads physical children in stored order followed
by all linked child kinds, applies visibility/ownership and filters, then replaces
eligible Movie/Series members with visible collections. Local alternate-version
suppression and the three name-range rechecks match the selected source consumer.
The implementation batches membership reads and uses the existing trait seams.

Existing/adopted movie and TV UserViews choose their source default kinds and
recursive/direct dispatch. Explicit item IDs first validate the actual parent,
retain any collection/playlist ancestor redirect and user restrictions, then clear
parent selection and bypass view defaults. Requested IDs always suppress collapse.
The real HTTP fixture exercises four view variants, both settings and recursion
choices, blocked parents/items, requested kinds and ordered pages/counts.

The source oracle is Jellyfin 4910aafa1a: Folder.GetItemsInternal and
CollapseBoxSetItems, UserViewBuilder.GetUserItems, CollectionManager, ItemsController
and BaseItemRepository query translation. Native Jellyfin 12.1.0 is separate runtime
evidence. S21 general sorting/request-ID order, S22 active-DVR keys, S24 missing
view producers and S25 BoxSet/music child overrides remain explicit work items.
S32 records the independently confirmed home-video resolver gap in the living audit;
the original failed native setup is retained as evidence. The reviewed native fixture
uses actual scanned MusicVideo items to distinguish recursive all-kind substitution
from the plain CollectionManager Movie/Series-only consumer.
This finding verifies the grouping switches on their actual consumers.
Recursive BoxSet/Playlist membership now follows the pinned linked-descendant
branch. Batched folder-root discovery preserves first-visit ancestry/link roles,
all direct link kinds, nested folders, cycles and root exclusion. It binds root
sets rather than materializing every leaf. Rows and counts reuse the resolved
scope; internal count queries use the same resolver. Physical ownership/deletion
keeps its separate scope. Real repository and HTTP cases cover direct/recursive
membership, Series/Episode descendants, user restrictions and pages/counts.
S25 retains collection DisplayOrder, modern physical-child and music overrides.

Production helpers separate option construction, parent dispatch, child loading,
substitution and post-filter sorting/paging. Strict lint checks remain enabled;
only coherent, large fixture matrices have individual length allowances.
The unplayed-series fixture persists its Episode ancestry closure, as a real scan
does; scalar OrdinalIgnoreCase ordering uses the measured Linux .NET oracle.
An offline oracle executes the exact pinned LINQ with EF SQLite 10.0.11 on
.NET 10.0.12/SQLite 3.53.3. All 166 ordinary/collapsed observations agree with
the implementation. SQLite lowercases ASCII on the row; the .NET bound uses
ToLowerInvariant. Three older uppercase Unicode prefix expectations are corrected
from that measurement, with lowercase and literal punctuation controls. The committed
83-case golden matrix verifies the real repository results, identities and counts
in both shapes. This SQL operation remains distinct from in-memory Folder matching.
Full reference SHA256: `05e938b771303330f35d96d7241c4b9d387c535efc8665581f13062a27bc68d7`.

Validation passed:

- fmt: passed.
- clippy: passed.
- core: 415 tests run: 415 passed, 1953 skipped.
- api: 102 tests run: 102 passed, 797 skipped.
- traits: passed.
- sql-boundary: 1 test run: 1 passed, 0 skipped.
- http: 1 test run: 1 passed, 0 skipped.
- http-folder-view: 1 test run: 1 passed, 0 skipped.
- date-oracle-http: 1 test run: 1 passed, 37 skipped.
- oracle: passed.
- build: passed.
- traits-descendant: 1 test run: 1 passed, 75 skipped.

Each changed nonexempt crate passed its own line-coverage gate:

- ferrofin-core: 114,997/120,722 lines (95.26%). Fresh affected tests and the recorded prior profile were merged and exported against current binaries. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: no LLVM diagnostics.
- ferrofin-api: 40,734/47,397 lines (85.94%). Fresh affected tests and the recorded prior profile were merged and exported against current binaries. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: warning: 53 functions have mismatched data.
  Earlier attempt/seed diagnostics retained: warning: 53 functions have mismatched data.

Evidence: `/tmp/ferrofin-dashboard-d02-checks.json`,
`/tmp/ferrofin-dashboard-d02-coverage.json`, and the passed native before/after/reference JSON records.
Storage recorded at the strict Clippy gate: 25.21 GiB generated target, 627.65 GiB free on the host.
Builds/coverage/native processes were serialized with one compiler job and
incremental/debug data disabled. The source worktree and commits are preserved.


Measured local before/after observations (milliseconds):

| Phase / consumer / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| disabled / ordinary_parent_timing | 2.848 / 3.405 | 3.187 / 5.040 | 5.770 / 8.750 |
| disabled / plain_parent_timing | 3.199 / 5.969 | 4.806 / 10.193 | 5.330 / 7.560 |
| disabled / combined_timing | 3.302 / 5.920 | 4.028 / 8.101 | 8.757 / 11.523 |
| movies_only / ordinary_parent_timing | 3.529 / 5.419 | 3.754 / 4.704 | 7.642 / 10.867 |
| movies_only / plain_parent_timing | 3.943 / 4.387 | 7.200 / 9.098 | 5.980 / 8.200 |
| movies_only / combined_timing | 4.173 / 5.030 | 4.195 / 7.401 | 9.741 / 12.965 |
| shows_only / ordinary_parent_timing | 3.156 / 6.386 | 2.984 / 4.787 | 5.703 / 7.684 |
| shows_only / plain_parent_timing | 3.417 / 5.427 | 5.273 / 6.387 | 5.335 / 7.907 |
| shows_only / combined_timing | 3.833 / 5.010 | 3.693 / 4.226 | 9.834 / 13.580 |
| both / ordinary_parent_timing | 3.096 / 6.005 | 2.657 / 5.576 | 6.537 / 7.938 |
| both / plain_parent_timing | 3.370 / 6.113 | 6.815 / 12.079 | 5.933 / 7.739 |
| both / combined_timing | 2.969 / 3.485 | 3.410 / 4.868 | 8.946 / 15.640 |
| disabled_again / ordinary_parent_timing | 2.768 / 3.127 | 3.030 / 5.754 | 5.733 / 10.609 |
| disabled_again / plain_parent_timing | 2.906 / 3.500 | 4.617 / 7.971 | 5.234 / 8.223 |
| disabled_again / combined_timing | 2.884 / 4.402 | 3.168 / 6.337 | 8.472 / 14.904 |

Paired values show median / p95 where the native fixture records repeated
HTTP reads; single values are the actual recorded request or completion time.
Before uses the preserved parent binary; after uses the verified current server. Each
run creates the same isolated data and exercises the same setting transitions.
Unavailable means that consumer was absent or did not complete in that run.
These are local observations on a shared host, not publishable benchmark claims.
The preferred Docker benchmark was unavailable; no quiet-host result is claimed.
The runtime reference is separate from the pinned source oracle.

Final source provenance: `/tmp/ferrofin-d02-source22-freeze-v2/freeze.json`
records the exact applied source and closed passing normal/native runs.
The 47-piece continuation queue retains earlier source and evidence snapshots;
all 25 successor patches apply strictly over this verified change.

Target-only cleanup reclaimed 7.11 GiB before coverage, leaving 18.18 GiB
at that point. The preserved parent binary, source worktree and commits remained
intact. Evidence: `/tmp/ferrofin-dashboard-d02-target-cleanup.json`.
