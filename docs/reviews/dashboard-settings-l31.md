# L31: trickplay extraction during scans

The scanner now invokes the shared trickplay manager after a selected video
refresh, using the live owning library's
`ExtractTrickplayImagesDuringLibraryScan`. This is independent of extraction
enablement: a selected refresh still discovers user grids or removes disabled
internal data. The manager shares eligibility, encoding limits and storage with
the scheduled task.

Live `TrickplayOptions.ScanBehavior` controls whether metadata completion waits
for extraction. Blocking waits; other enum values start a background operation.
`RegenerateTrickplay` forces replacement only in FullRefresh. A successful new
probe supplies its actual streams, including an empty result; a refresh without
a new probe reads stored streams. New videos are saved before the trickplay
foreign key is used. Existing blocking refreshes retain their metadata Etag
until extraction finishes.

Cancellation stops completion and drops extraction work. Temporary frame and
tile directories use drop guards, including cancellation while encoding.
Ordinary custom-provider errors are logged but do not prevent
`DateLastRefreshed`, matching `MetadataService.RunCustomProvider`. The HTTP
error fixture uses a real SQLite discovery failure; an unreadable JPEG is not
an error in the pinned image decoder.

Selection follows run-all and the actual media-file change monitor. Subtitle
changes and the separate metadata-backfill heuristic are not treated as that
monitor. Generic provider change-monitor dispatch remains open work: extend the
provider traits and refresh planner with actual per-provider `HasChanged`
results and feed custom-provider selection without reinterpreting backfill.
The pinned built-in remote providers do not implement that interface. This
setting does not establish a generic provider-monitor implementation (S27).

Pinned source oracle: Jellyfin `4910aafa1a`,
`MediaBrowser.Providers/Trickplay/TrickplayProvider.cs`,
`MediaBrowser.Providers/Manager/MetadataService.cs`, and
`Jellyfin.Server.Implementations/Trickplay/TrickplayManager.cs`.
Native Jellyfin 12.1.0 is separate runtime evidence.

Validation passes: 35 focused core tests and the fresh full 2,318-test core
suite, three real-server HTTP trickplay tests, SQL boundary checks, trait
compilation, normal server build, formatting and strict workspace/
all-target/all-feature Clippy.
The before binary produces no scan-time tiles when the setting is enabled.
All seven after/reference catalog and destination gates pass, including live
on/off changes, reuse, replacement and disabled cleanup. The HTTP held-encoder
fixture proves the different Blocking/NonBlocking save boundaries. Fresh full core line coverage is **95.23%** (110,332 / 115,862), with zero
LLVM diagnostics. The initial combined profile reached 78.32% and was
rejected; the passing run uses no earlier seed. Traits/server are coverage
exempt. The complete batch workspace/doctest gate remains required.

| Native phase | Before (ms) | After (ms) | Jellyfin reference (ms) |
| --- | ---: | ---: | ---: |

| scan flag disabled | 246.11 | 242.56 | 94.45 |
| blocking scan generates | 232.30 | 229.07 | 167.46 |
| blocking scan reuses | 229.84 | 233.68 | 99.94 |
| nonblocking scan replaces | 229.81 | 246.50 | 86.26 |
| scan flag disabled again | 222.98 | 235.55 | 84.36 |
| scan disabled extraction prunes | 230.40 | 235.30 | 82.71 |
| full refresh forces when disabled | 225.26 | 291.73 | 87.37 |

These are one observation per phase on a shared host, including 50ms
completion polling. They measure metadata completion and do not claim a
general performance improvement or a NonBlocking extraction-speed comparison.

Evidence: `/tmp/ferrofin-dashboard-l31-checks.json`,
`/tmp/ferrofin-dashboard-l31-coverage.json`, and
`/tmp/ferrofin-dashboard-l31-native-{before,after,reference}.json`.

Target after validation: 23.86 GiB; host free space: 671.44 GiB.

L32 revalidation repaired the shared HTTP fixture’s internal data root and added
a successful cleanup control after removal of the injected database error.
All four trickplay HTTP tests now pass with discovery demonstrably reached;
see `dashboard-settings-l32.md`. The earlier held-manager unit evidence remains
valid, while this replaces the earlier error-injection fixture evidence.
