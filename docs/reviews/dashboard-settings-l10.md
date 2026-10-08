# L10: embedded video titles and episode information

The scanner now honors `EnableEmbeddedTitles`, the additional
`EnableEmbeddedExtrasTitles` gate for extras, and `EnableEmbeddedEpisodeInfos`.
Video probing updates the existing metadata before local-provider merging and
respects item and Name locks for titles. NFO episode numbers override probe
numbers. The generic video prober still fills missing episode numbers regardless
of the episode-info switch, matching upstream.

Source: Jellyfin `4910aafa1a`, `FFProbeVideoInfo.FetchEmbeddedInfo`,
`Episode.BeforeMetadataRefresh`, `MetadataService.RefreshMetadata`, and
`LibraryManager.FindExtras`. The episode pre-refresh gate requires a stored
container equal to `mp4`; a normal comma-separated ffprobe format list does not
qualify. Extras can return to their filename on a quiet scan when the prober
does not run. Both behaviors are confirmed against the native reference.

Validation: 412 focused core tests pass, including all change-detection
integration tests, title and episode gates, locks, NFO precedence, and missing
metadata refresh. Four native HTTP phases match Jellyfin 12.1.0 exactly across
five movie libraries and two episode libraries: initial scan, quiet scan,
changed-file missing-metadata refresh, and persisted `mp4` container refresh.
Formatting, SQL boundary, server build and strict workspace Clippy pass.

Separate core coverage using the L09 baseline and 412 newly instrumented tests:
**94.73%** (102,591 / 108,297 lines), using 22 current binaries. Existing LLVM
function-data mismatch warnings remain. The four host file-watch-limit failures
in the full L07 core run remain an outstanding workspace gate.

Native parent/updated debug runs on the same fixtures measured four completed
scan operations each: median **677 → 687.5 ms**, ranges **249–1,195 → 191–842**.
These small shared-host observations are not production performance estimates.

Evidence: `/tmp/ferrofin-dashboard-l10-checks.json`,
`/tmp/ferrofin-dashboard-l10-coverage.json`,
`/tmp/ferrofin-dashboard-l10-native-final.log`, and
`target/dashboard-test-tmp/ferrofin-dashboard-l10-{ferrofin-_nxqlyfb,jellyfin-_6wdidwh}/results.json`.
