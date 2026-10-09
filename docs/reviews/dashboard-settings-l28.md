# L28: enable chapter images

The scheduled chapter task now reconciles disabled and previously failed
videos instead of leaving stale image references and files behind. It shares
one extractor with the upcoming scan-time consumer. Extraction uses the live
owning library, physical/complete-video eligibility, the persisted default
video-stream index, source chapter-density rules, and DateModified-based cache
filenames. Fresh video scans now persist that required default index; this
prerequisite belongs in L28 so the task works before L29.

The first frame uses min(15 seconds, runtime), and chapters below runtime can
extract near EOF. Existing images can be adopted without new extraction;
references and unused supported image files reconcile. Repository/service
errors propagate without poisoning extraction-failure history. Genuine earlier
failures still flush on a later error. Existing systemic write-permission
preflight remains, while disabled-only cleanup does not require encoder output.

Pinned source: Jellyfin `4910aafa1a`, ChapterManager, ChapterImagesTask,
PathManager and FFProbeVideoInfo. The shared helper has no strong library or
chapter-manager ownership and only weak DVR ownership. The task includes owned
videos. No production SQL query node is added; the scanner ratchet stays 24.

Native baseline: a fresh 36-second video with chapters at 0/26/35/runtime,
scanned with both flags off. Scheduled enable produced images at 0/26, skipped
35 and retained all four chapter rows. Both images were already blue, so this
finding does not claim a baseline first-frame color defect. Disabling retained
the two images and their references; re-enabling reused them. Timings:
initial scan 426.06 ms, enabled 107.73 ms, disabled cleanup 26.91 ms,
re-enabled 27.17 ms.

The separate during-scan flag and event-driven scheduling remain L29. L28
native proof invokes only the scheduled task; it does not require L29's removal
of queued whole-library extraction or the old scanner's at-runtime chapter row.
Dummy marker generation and configured image dimensions remain M03/M04.

Evidence: `/tmp/ferrofin-dashboard-l28-native-before.json`; runners
`/tmp/ferrofin-dashboard-l28-checks.py` and
`/tmp/ferrofin-dashboard-l28-coverage.py`; native fixture
`/tmp/ferrofin-dashboard-l28-l29-live.py --finding l28`.

Final changed-binary HTTP proof produces all three below-runtime images, including
35 seconds, removes the runtime marker when saving, clears references and files
when disabled, and regenerates them when re-enabled. Cache paths include the
file's DateModified ticks. The independent Jellyfin 12.1.0 run agrees on these
scheduled-task outcomes; the behavioral source oracle remains the pinned
12.0-rc7 commit. Initial disabled-scan marker removal belongs to L29.

| Native phase | Before (ms) | After (ms) | Jellyfin reference (ms) |
| --- | ---: | ---: | ---: |
| Initial disabled scan | 426.06 | 476.56 | 455.17 |
| Scheduled enable | 107.73 | 129.59 | 178.02 |
| Scheduled disabled cleanup | 26.91 | 29.34 | 22.99 |
| Scheduled re-enable | 27.17 | 133.16 | 147.53 |

Each phase is one local observation on the shared host, including task polling.
The enabled and re-enabled tasks now perform the required extra encoding work;
these measurements are not a general performance improvement claim.

Validation: 45 focused core tests, 43 image API integration tests, the SQL
boundary ratchet, real-server metadata-locale HTTP, trait compilation, normal
server build, formatting and strict workspace/all-target/all-feature Clippy
pass. Fresh affected core execution merged with the verified L27 full-core
profile reports **83.09%** line coverage (94,639 / 113,903), above the separate
80% gate, with no LLVM diagnostics. Traits/server are coverage-exempt. The
final whole-workspace test and doctest gate remains required for this batch.

Final evidence: `/tmp/ferrofin-dashboard-l28-checks.json`,
`/tmp/ferrofin-dashboard-l28-coverage.json`,
`/tmp/ferrofin-dashboard-l28-native-after.json`,
`/tmp/ferrofin-dashboard-l28-native-reference.json`. The target directory was
23.29 GiB after validation, with about 682 GiB host space free.
