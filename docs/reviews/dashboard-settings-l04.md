# L04: disabling photos removes existing catalog entries on scan

`EnablePhotos=false` already stopped discovering new photos in home-video
libraries, but the stale-item safeguard retained existing photos because their
files still existed. After a scan, those photos remained browsable indefinitely.
The safeguard now recognizes that the saved option invalidates both Photo and
PhotoAlbum resolution. It removes their database rows while preserving files,
video entries and the existing unavailable-location/cascade protections.
Re-enabling photos discovers the same files again under their deterministic IDs.

Source: Jellyfin `4910aafa1a`, `PhotoResolver.Resolve`,
`PhotoAlbumResolver.Resolve` and folder child validation. This option applies on
the next scan; saving the setting does not itself request a scan.

All **eight native observations match Jellyfin 12.1.0**: off/on/off/on for flat
photos and for a nested album. Disabling now removes previously indexed entries,
a new file stays unindexed while disabled, all source files remain present,
and re-enabling restores photos/albums. Native fixtures:
`/tmp/ferrofin-dashboard-l04-ferrofin-kc67wh58/results.json` and
`/tmp/ferrofin-dashboard-l04-ferrofin-kimp8a3o/results.json`.

Validation passes: **279 scanner tests**, **43 location/adoption/watcher scan
tests**, SQL boundary, formatting, strict workspace Clippy and server build.
The new regressions cover album removal, video preservation, re-enabling and
an unavailable media root: disabling does not prune it until it is reachable.

Core's full coverage run passes **2,162 tests** and retains four host inotify
watcher failures. The separate threshold passes; the 22 current test binaries
report **95.00%** (102,114 / 107,493 lines), with 19 LLVM mismatched-function
warnings. These host-dependent failures remain the L01 workspace limitation.

The native server's measured complete scan times for initial-off, on, off with
a new file, and on again were respectively **177 → 188 ms**, **619 → 630 ms**,
**173 → 186 ms**, and **194 → 208 ms**. Each is one small-fixture debug scan on a
shared host; these observations verify the changed path and do not establish
production performance. The disabled scan now performs the required deletion
of catalog rows instead of retaining them.

Evidence: `/tmp/ferrofin-dashboard-l04-checks.json`,
`/tmp/ferrofin-dashboard-l04-coverage.json`,
`/tmp/ferrofin-dashboard-l04-exact-coverage-summary.json`, and parent fixture
`/tmp/ferrofin-dashboard-l04-ferrofin-l5dmrmg9/server.log`.
