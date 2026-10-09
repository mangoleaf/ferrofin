# D01: display folder view

The live EnableFolderView switch now adds/removes the localized Folders home
view. Its identity uses the named metadata directory, surviving language changes;
adopted numeric ViewType and tokenized paths remain usable without warm writes.
Existing hidden-view preferences and physical library visibility apply.

Browsing that view selects visible immediate root children. The shared typed
filter implements the pinned GetResult whitelist, including own user-data key
precedence, nullable Likes/Rating and leaf/folder played semantics, literal name
prefix/ranges, image index zero and nonempty provider values. It preserves input
order without explicit sorting; explicit Name/SortName sorting is stable and
ordinal case-insensitive, including forced-name precedence. Filtering and
adjacency precede total-count calculation and paging. Direct IDs bypass the
view builder and retain the ordinary parent scope; nil adjacency is absent.

Pinned source is Jellyfin 4910aafa1a: UserViewManager, UserViewBuilder.GetResult,
Folder.GetChildren, BaseItem keys and registered Name/SortName comparers.
API consumers use traits; core implements the batched persistence algorithms.

The independent additional work remains concrete in the living document:
S21 general ordering and generated keys, S22 active-DVR key override, S23 named
view artwork/refresh lifecycle, and S24 grouped/preset producers. This finding
verifies the display switch and browsing; it does not close those consumers.

The cold real-server fixture exposed a missing plugin-root provision: Folders
must include Playlists before any MediaFolders request or scan. The view now
settles the existing root/plugin stores on first use; warm reads remain
idempotent. The native fixture independently asserts this cold membership.

HTTP name boundaries reproduce ASP.NET10 blank-to-null string binding without
trimming nonblank input. Internal typed filters still treat whitespace
literally. Both /Items and user-scoped Items routes cover all three boundaries.

The executable .NET10 name oracle also rejected the initial supplementary
character ordering assumption. The corrected OrdinalIgnoreCase consumer uses
simple invariant casing and scalar order for valid Rust strings, retaining
null ordering and stable ties. BMP versus supplementary characters, Deseret
case equivalence, common prefixes and sharp-S are now explicit regressions.
The primary runtime implementation is OrdinalCasing.Icu.cs173–265 in dotnet
v10.0.0; this is distinct from ordinary Ordinal UTF-16 comparison.

Repository-only test helpers retain the adopted historical save date and stored
sort-key fixtures; the SQL boundary was not relaxed. Seeded CollectionFolder
fixtures now carry the real IsFolder fact.

The native rated-filter case exposed eight missing HTTP bindings. Both Items
routes now forward AdjacentTo, HasOverview, the three provider-presence flags,
HasParentalRating, and both premiere-date bounds. Nullable blank inputs, UTC
offsets, malformed inputs, and filtering before count/adjacency/paging have
real API and Folders regressions. The source-only internal inputs remain
unexposed.

Items premiere-date bounds now use their own binder. Offsetless dates are
interpreted in the server's local timezone, matching ItemsController's
ToUniversalTime conversion; explicit offsets are converted during binding.
DST gaps/folds select standard time, valid local conversions clamp at the
DateTime limits, and fractional seconds reproduce the source's floating-point
accumulation and rounding to 100ns. Invalid offsets, leap seconds and binding
range overflow are rejected. LiveTV retains its separate binder.

The .NET 10.0.12 reference matrix records 320 cases across eight zones with
host tzdata 2026c, including historical offsets, negative/half-hour DST,
precision, range and rejection cases. A real TCP test exercises both Items
routes in owned child processes, changing only each child's TZ. The production
fixture independently sets a collection's premiere date through HTTP and checks
five Folders date/count boundaries in America/New_York, including a single tick.
The measured platform is Linux x86_64; this establishes the tested ISO input
forms and does not establish all DateTime.Parse formats or historical timezone
parity on experimental Windows/macOS targets.

Reference implementation:
[ASP.NET DateTime binder](https://github.com/dotnet/aspnetcore/blob/v10.0.0/src/Mvc/Mvc.Core/src/ModelBinding/Binders/DateTimeModelBinder.cs),
[DateTimeParse](https://github.com/dotnet/runtime/blob/v10.0.0/src/libraries/System.Private.CoreLib/src/System/Globalization/DateTimeParse.cs),
and [TimeZoneInfo Unix conversion](https://github.com/dotnet/runtime/blob/v10.0.0/src/libraries/System.Private.CoreLib/src/System/TimeZoneInfo.Unix.cs).
The existing workspace libc 0.2.189 edge is reused. Latest upstream
[libc documentation](https://docs.rs/crate/libc/latest) lists 0.2.190; no unrelated
package upgrade was introduced.

The new direct dependency edges reuse locked versions. Maintainer-published
latest release documentation confirms [icu_collator 2.3.1](https://docs.rs/crate/icu_collator/latest)
and [unicode-normalization 0.1.25](https://docs.rs/crate/unicode-normalization/latest);
no package upgrade or new version resolution was needed.

Validation passed:

- fmt: passed.
- core: 346 tests run: 346 passed, 1999 skipped.
- api: 102 tests run: 102 passed, 797 skipped.
- traits: passed.
- sql-boundary: 1 test run: 1 passed, 0 skipped.
- http: 1 test run: 1 passed, 0 skipped.
- oracle: passed.
- clippy: passed.
- build: passed.
- date-oracle-http: 1 test run: 1 passed, 37 skipped.

Each changed nonexempt crate passed its own line-coverage gate:

- ferrofin-core: 113,203/118,796 lines (95.29%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: no LLVM diagnostics.
- ferrofin-api: 40,691/47,265 lines (86.09%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: warning: 53 functions have mismatched data.
  Earlier attempt/seed diagnostics retained: warning: 53 functions have mismatched data.

Evidence: `/tmp/ferrofin-dashboard-d01-checks.json`,
`/tmp/ferrofin-dashboard-d01-coverage.json`, and the passed native before/after/reference JSON records.
Storage recorded at the strict Clippy gate: 19.02 GiB generated target, 672.55 GiB free on the host.
After final API coverage, generated target used 24.30 GiB with 671.23 GiB free.
The pre-coverage cleanup removed 8.63 GiB of stale generated artifacts.
Fresh full coverage suites passed all 2,345 core and 899 API tests.
Builds/coverage/native processes were serialized with one compiler job and
incremental/debug data disabled. The source worktree and commits are preserved.


Measured local before/after observations (milliseconds):

| Phase / consumer / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| disabled / ordinary_parent_browse | 4.003 / 6.364 | 3.059 / 4.147 | 9.907 / 14.804 |
| enabled / ordinary_parent_browse_timing | 4.525 / 7.876 | 3.202 / 4.179 | 8.668 / 11.610 |
| localized / ordinary_parent_browse_timing | 4.709 / 6.899 | 3.101 / 3.917 | 7.784 / 11.277 |
| disabled_again / ordinary_parent_browse_timing | 3.438 / 5.028 | 3.020 / 3.415 | 8.468 / 12.268 |
| reenabled / ordinary_parent_browse_timing | 3.409 / 4.346 | 3.049 / 4.449 | 7.987 / 11.338 |
| enabled / folders_browse_timing | unavailable | 3.071 / 3.745 | 8.047 / 11.622 |
| localized / folders_browse_timing | unavailable | 2.834 / 4.209 | 6.196 / 10.114 |
| reenabled / folders_browse_timing | unavailable | 2.778 / 4.210 | 5.898 / 9.318 |

Paired values show median / p95 where the native fixture records repeated
HTTP reads; single values are the actual recorded request or completion time.
Before uses the preserved parent binary; after uses the repaired server. Each
run creates the same isolated data and exercises the same setting transitions.
Unavailable means that consumer was absent or did not complete in that run.
These are local observations on a shared host, not publishable benchmark claims.
The preferred Docker benchmark was unavailable; no quiet-host result is claimed.
The runtime reference is separate from the pinned source oracle.
