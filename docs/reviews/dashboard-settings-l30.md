# L30: trickplay extraction enablement

The scheduled task now reads the live physical owning library for each candidate
video, includes owned extras and excludes nonvideo/virtual/folder rows.
The declared SourceTypes query field is inert in the pin and is not reinterpreted
as a channel exclusion: complete channel VOD remains a candidate. Shared eligibility uses the source's actual disc/placeholder/shortcut,
active recording/channel livestream, runtime and any-stream rules before any
discovery or pruning. Audio-only candidates can undergo outer cleanup but
cannot encode a video frame; no chapter-default-index requirement is invented.

Existing user-managed grids are discovered before the extraction flag. Valid
arbitrary width/grid dimensions, actual image size, inferred interval, padded
capacity and bandwidth are persisted. Raw configured interval is used during
discovery; generation's minimum interval is applied later. Existing rows with
no directory at either location are pruned. Disabled internal images are
removed, while disabled media-side user tiles are catalogued and preserved.
Missing media files and backdrops are checked per width after outer discovery
and disabled cleanup, matching their source call placement.

Pinned source oracle: Jellyfin 4910aafa1a,
Jellyfin.Server.Implementations/Trickplay/TrickplayManager.cs,
MediaBrowser.Providers/Trickplay/TrickplayImagesTask.cs and
MediaBrowser.Model/Dto/MediaSourceInfo.cs. Native Jellyfin 12.1.0 is separate
runtime evidence. During-scan and destination consumers remain L31/L32.

Validation: 27 focused core tests, SQL boundary, trait compilation, real-server
HTTP trickplay integration, normal build, formatting and strict workspace/
all-target/all-feature Clippy pass. Independent native before/after runs use the
same isolated four-second video and 160px 2x2 grid. The before binary cannot
serve the disabled user grid; the after binary catalogs and serves it without
modifying tile bytes or mtime. Generation, reuse and disabled internal cleanup
remain correct. All six after/reference observation gates pass. The independent
Jellyfin runtime is 12.1.0; the source oracle remains 12.0-rc7.

Source recheck preserves two non-obvious outcomes: SkiaEncoder returns zero
dimensions for an unreadable JPEG, and discovery catalogs it with minimum
height one; a real video-stream width zero skips generation, while a missing
Width does not cap it. Regression assertions were corrected to those source
outcomes. The obsolete first-location helper is removed. Existing persistence
helpers seed the new Data cases, retaining the trickplay SQL ceiling of seven.

| Native phase | Before (ms) | After (ms) | Jellyfin reference (ms) |
| --- | ---: | ---: | ---: |
| Disabled initial | 52.36 | 53.38 | 118.37 |
| Enabled generation | 107.89 | 113.62 | 130.61 |
| Enabled reuse | 52.13 | 52.67 | 62.07 |
| Disabled internal pruning | 52.05 | 52.46 | 27.69 |
| Disabled user-grid discovery | 52.07 | 54.52 | 64.59 |
| Disabled user-catalog reuse | 54.11 | 53.88 | 26.69 |

These are one observation per phase on a shared host, including 50ms task
polling; they are not a general performance improvement claim.

Separate core line coverage: **80.8%** (93,083 / 115,208), with 0 LLVM diagnostics. Fresh affected tests are mandatory; a verified earlier profile contributes compatible unchanged execution. The full batch workspace/doctest gate remains required. Traits/server are coverage-exempt.

Evidence: `/tmp/ferrofin-dashboard-l30-checks.json`,
`/tmp/ferrofin-dashboard-l30-coverage.json`,
`/tmp/ferrofin-dashboard-l30-native-{before,after,reference}.json`.
Target after validation: 23.59 GiB; host free space remains above 674 GiB.
