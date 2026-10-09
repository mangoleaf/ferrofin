# L05: realtime monitoring follows saved library options

`EnableRealtimeMonitor` already filters virtual-folder locations, and the
LibraryOptions HTTP handler stops and restarts the monitor after saving. The
restart reads current options. This review additionally found that overlapping
and repeated locations were each recursively watched. The monitor now sorts
roots and suppresses roots covered by an earlier parent, using directory
boundaries and invariant ordinal casing, as Jellyfin does.

Source: Jellyfin `4910aafa1a`, `LibraryMonitor.Start`,
`IsLibraryMonitorEnabled`, and `ContainsParentFolder`.

The 50 focused monitor/virtual-folder tests pass, including new regressions for
saved on/off/on changes, a shared path still needed by another enabled library,
all libraries disabled, duplicate roots, descendants and similarly prefixed
siblings. Formatting, SQL boundary, strict workspace Clippy and build pass.

The rebuilt native server passed off/on/off through real HTTP: newly created
files stayed absent with monitoring disabled, appeared without a manual scan
after enabling it, and stayed absent after disabling it again. The initial
parent-build attempt could not create an inotify watch because the host quota
was exhausted; the rebuilt run succeeded after the host condition cleared.
No host limits or unrelated processes were changed.

Single-request option-save timings were off **3.43 → 3.77 ms**, on
**3.67 → 3.81 ms**, and off again **3.48 → 4.41 ms**. These small debug fixture
observations include different host watch availability and are not a production
performance comparison. Production code changes only reduce redundant watches.

Native evidence: `/tmp/ferrofin-dashboard-l05-ferrofin-35xrmhj_/results.json`
(parent) and `/tmp/ferrofin-dashboard-l05-ferrofin-jk2x7y9c/results.json` (after).
Checks: `/tmp/ferrofin-dashboard-l05-checks.json`.

Core coverage: **2,164 tests pass**, with the same four intermittent host
inotify failures. The current 22 binaries report **95.00%** line coverage
(102,182 / 107,563); the exact export retains LLVM mismatched-function warnings.
The broad stale-binary report was stopped after the current-binary export
passed. This does not make the outstanding workspace watcher gate green.
Evidence: `/tmp/ferrofin-dashboard-l05-exact-coverage-summary.json` and
`/tmp/ferrofin-dashboard-l05-ferrofin-core-coverage.log`.
