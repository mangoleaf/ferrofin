# D05: date added from file creation time

Linux file-creation dating now matches the source runtime's filesystem clock.
Ferrofin previously preferred statx birth time, while .NET 10.0.12 on Linux uses
native stat without birth-time support and selects the older of inode change
time and modification time. A copied file with a preserved old modification
time was therefore dated by its new birth time instead of the preserved date.
`FileTimes::of` now supplies the Linux source backend's inputs; other operating
systems retain their native creation-time input. The pure birth/fallback
selection helper is unchanged.

The live named metadata option `UseFileCreationTimeForDateAdded` applies to both
new imports and existing physical files whose modification time changes. The
living document's earlier “newly dated items” restriction also needs correction.

New imports use file creation time when enabled, with the source fallback for
an unusable creation date; disabled imports use current import time. On an
existing file change, enabled refresh replaces DateCreated with a valid creation
time, while disabled refresh preserves its saved date. Saving configuration or
refreshing an unchanged file does not itself re-date existing rows. The source
change threshold is an absolute modification-time difference greater than one
second. Folder filesystem inputs do not supply the same file timestamps.

Core cases cover changed/unchanged files, the exact one-second boundary,
missing timestamps, folders and disabled behavior. HTTP/native cases import
under false → true → false and compare existing changed/unchanged refreshes.
Import-time dates must fall within the actual fresh scan interval; creation-time
dates must match a fresh filesystem stat. An editor-seeded
`2001-02-03T04:05:06.1234567Z` must survive unchanged refreshes at 100ns precision.
Every explicit refresh requires both the new NFO title and a new Etag; scans
require a known task's new EndTimeUtc plus Completed status. Time comparisons use
integer ticks, and creation time is re-statted after each file edit.

The Linux regression uses a real file whose modification time is preserved at
2020-01-01 and independently asserts that exact timestamp. The real scan
integration suite distinguishes preserved modification time from birth time
and is selected explicitly for normal tests and fresh affected coverage. The
native fixture checks Ferrofin against Linux stat inputs and Jellyfin against
a small offline BCL oracle reading `FileInfo.CreationTimeUtc` directly. The
oracle's exact 100ns result matched the failed reference observation; neither
comparison uses a tolerance. Raw birth time remains a diagnostic.

The initial fixture posted only DateCreated through the full metadata editor,
which correctly cleared Name in both implementations. It now submits the
current DTO with only DateCreated changed and independently verifies that Name
and all seven fractional date digits survive. Each refresh uses
`replaceAllMetadata=true`: source FullRefresh without replacement only fills
missing metadata and cannot replace the existing title used as its completion
signal. Title-only NFOs have no dateadded value, so replacement retains the
date-policy assertions. A test-only undeclared chrono reference was replaced
with the standard clock, and a redundant struct-default update was removed
after strict Clippy rejected it. The original failures are retained under
`/tmp/ferrofin-d05-root-failure-evidence/`.

Source contract: Jellyfin `4910aafa1a`, `ResolverHelper.SetDateCreated`,
`MetadataService.BeforeSaveInternal`, `BaseItemExtensions.HasChanged`,
`ManagedFileSystem` and `ItemUpdateController`. The measured runtime is .NET
10.0.12, with its exact CoreLib version recorded by the oracle. Its Linux
[native stat backend](https://github.com/dotnet/runtime/blob/v10.0.12/src/native/libs/System.Native/pal_io.c#L185)
and [creation-time fallback](https://github.com/dotnet/runtime/blob/v10.0.12/src/libraries/System.Private.CoreLib/src/System/IO/FileStatus.Unix.cs#L267)
establish the platform clock. Native Jellyfin 12.1.0 is separate runtime
evidence. The fresh import/refresh fixture does not measure the pinned
one-off RefreshInternalDateModified migration. Filesystem creation-time behavior
is evaluated on the host platform, including its available birth-time/fallback
semantics.

[S03 and S04](../../brain/knowledge/JELLYFIN_WEB_DASHBOARD_SETTINGS_REVIEW.md#additional-source-resolution-finding-from-implementation)
remain separate physical-hierarchy and general rescan-metadata work. These date
assignment checks do not establish every resolver or unlocked metadata merge.
The fill-only title observation is source behavior and does not establish a
general metadata merge defect.

Required validation includes core tests/coverage >=80%, SQL boundary, real HTTP,
production/native comparisons, build, final-milestone doctests and strict workspace/all-targets/
all-features Clippy. Final provenance is
`/tmp/ferrofin-d05-source31-freeze-v4/final-freeze.json` (SHA-256
`8eb93a97abc39681742693895fce28a2752acf0b7aefabe1ab2257ee6957604f`). It
binds all 34 source files to the root-tested tree, the source31 Linux correction,
19 strictly applying unchanged successors, nine passing normal gates including
all 32 expanded core tests, and owned before/reference/after runs with no
surviving children. It also retains the actual offline oracle build and measured
.NET 10.0.12 creation-time observation. The normal checks record keeps its raw
source30 manifest; the final snapshot supplies the independently reviewed
correction without rewriting that history. The actual coverage run is recorded
below.

Final runner routing is verified in
`/tmp/ferrofin-d05-source50-manifest-preparation/final-complete-runner-list-checks.json`
(SHA-256 `b079a6aff2737d2ea317ebf398db98de4fe567cc04957d48193ddd77f7c43cf1`):
all 16 read-only checks/coverage list modes and 323 source/support artifact
hashes passed. Both D05 filters include `binary(library_scan_date_added)`.
The independent NFO manifests use the same source50 queue. D05 native roles
select the final Linux fixture and the owned wrapper, which binds the exact
published BCL oracle and adds it only to Jellyfin invocations.

Validation passed:

- fmt: passed.
- core: 32 tests run: 32 passed, 2351 skipped.
- sql-boundary: 1 test run: 1 passed, 0 skipped.
- http: 1 test run: 1 passed, 0 skipped.
- build: passed.
- clippy: passed.

Each changed nonexempt crate passed its own line-coverage gate:

- ferrofin-core: 106,022/121,730 lines (87.10%). Fresh affected tests and the recorded prior profile were merged and exported against current binaries. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: no LLVM diagnostics.

Evidence: `/tmp/ferrofin-dashboard-d05-checks.json`,
`/tmp/ferrofin-dashboard-d05-coverage.json`, and the passed native before/after/reference JSON records.
Storage recorded at the strict Clippy gate: 20.54 GiB generated target, 497.99 GiB free on the host.
Builds/coverage/native processes were serialized with one compiler job and
incremental/debug data disabled. The source worktree and commits are preserved.


Measured local before/after observations (milliseconds):

| Phase / consumer / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| Existing date phase 0 / refresh_ms | 244.395 | 229.514 | 62.374 |
| Existing date phase 1 / refresh_ms | 240.592 | 217.852 | 60.200 |
| Existing date phase 2 / refresh_ms | 224.360 | 216.083 | 59.448 |
| Existing date phase 3 / refresh_ms | 229.667 | 225.747 | 57.847 |
| Existing date phase 4 / refresh_ms | 229.275 | 224.199 | 56.120 |

Paired values show median / p95 where the native fixture records repeated
HTTP reads; single values are the actual recorded request or completion time.
Before uses the preserved parent binary; after uses the verified current server. Each
run creates the same isolated data and exercises the same setting transitions.
Unavailable means that consumer was absent or did not complete in that run.
These are local observations on a shared host, not publishable benchmark claims.
The preferred Docker benchmark was unavailable; no quiet-host result is claimed.
The runtime reference is separate from the pinned source oracle.
