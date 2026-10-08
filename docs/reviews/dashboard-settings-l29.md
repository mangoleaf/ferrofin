# L29: chapter images during library scans

Successful video Default/FullRefresh now reconciles chapters during the video's
own refresh. The live library's ExtractChapterImagesDuringLibraryScan controls
new frames independently of EnableChapterImageExtraction. Existing valid images
can be reused with during-scan extraction off; disabling the owning library
clears stale references and unused supported files. Refresh replaces authoritative
chapter rows, including empty sets, and omits markers at or beyond runtime.
Failed probes, audio, None and ValidationOnly preserve the prior chapter set.

The shared extractor owns no library/chapter manager. The composition root
attaches it to the scanner and removes the LibraryChanged subscriber that
queued whole-library extraction irrespective of the during-scan flag. The
scheduled L28 task remains independently available. No new SQL query node or
ratchet ceiling is introduced.

Cancellation follows the pinned call placement: before a new extraction it
preserves stored rows and prior files, leaving any completed new frame on disk;
an interruption caught inside the encoder follows the ordinary partial-failure
save/prune path. Cached rows can reconcile without new encoding. Real SQLite
regressions check each case and the live off/on/disabled transitions.

Source oracle: Jellyfin 4910aafa1a (12.0-rc7),
MediaBrowser.Providers/MediaInfo/FFProbeVideoInfo.cs Fetch and
Emby.Server.Implementations/Chapters/ChapterManager.cs RefreshChapterImages and
SaveChapters. DummyChapterDuration and configured image resolution remain
independent M03/M04 findings.

The preserved L28 binary's real HTTP scan baseline produces no images in all
three flag combinations (enabled/off, enabled/on, disabled/on) and retains an
at-runtime marker. Initial scan: 430.50 ms; changed-file phases:
370.68 / 371.00 / 371.67 ms. This fixture did not observe the old queued task
executing; the event-handler removal is also established by source review.

The changed binary produces three images only for enabled/on, adopts no new
frames for enabled/off, clears references/files for disabled/on, and never queues
the separate chapter task. All phases now contain three below-runtime rows.
Changed-file phase times: 371.86 / 479.65 / 369.26 ms; initial scan 436.48 ms.
The enabled/on increase includes three required frame encodes. These are single
local observations on a shared host with polling, not a general latency claim.

52 focused core tests, 43 image API integration tests, SQL boundary, traits,
real-server metadata-locale HTTP, normal build, formatting and strict
workspace/all-target/all-feature Clippy pass. Composition-root attachment uses
the retained scanner Arc after construction; the stale local binding was caught
and corrected by the actual server build.

The independent Jellyfin 12.1.0 reference produces the same below-runtime
chapter sets and flag-controlled image outcomes. Initial scan 481.05 ms;
changed-file phases 286.60 / 425.10 / 283.55 ms. Source and runtime pins remain
separate.

Fresh affected core tests merged with the verified L28 core profile report
**81.99%** line coverage (93,780 / 114,377), above the separate 80% gate,
with 0 LLVM diagnostics. Traits/server are coverage-exempt.
Final whole-workspace tests/doctests remain required for the batch. Baseline evidence: /tmp/ferrofin-dashboard-l29-native-before.json.

Final evidence: `/tmp/ferrofin-dashboard-l29-checks.json`,
`/tmp/ferrofin-dashboard-l29-coverage.json`,
`/tmp/ferrofin-dashboard-l29-native-{before,after,reference}.json`.
Target: 23.29 GiB; host free space: about 682 GiB.
