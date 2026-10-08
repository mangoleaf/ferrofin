# L09: season zero uses the saved display name

Physical and virtual season-zero discovery now uses `SeasonZeroDisplayName`.
Subsequent scans and direct season refreshes apply the saved name, preserving
item locks, Name field locks and ordinal case-only differences. Empty configured
names are retained. Episodes take their finalized parent season's actual name
when they refresh, including a locked custom name; refreshing only the season
does not rewrite its episodes.

Source: Jellyfin `4910aafa1a`, `SeasonResolver`,
`SeriesMetadataService.GetValidSeasonNameForSeries`, and
`SeasonMetadataService.BeforeSaveInternal` / `EpisodeMetadataService.BeforeSaveInternal`.

Validation: all 449 focused core/provider tests pass, including physical and
virtual quiet rescans, name locks, Unicode casing and empty names. Fifty native
HTTP observations match Jellyfin 12.1.0: five series across initial discovery,
saved-but-unscanned options, scan, case-only change and direct virtual-season
refresh, checking both season names and episode labels. Formatting, SQL
boundary, server build and strict workspace Clippy pass.

Separate coverage using the full L07 baseline and 449 newly instrumented tests:
core **94.70%** (102,341 / 108,070 lines), providers **93.72%**
(21,396 / 22,830). Current binaries only; 19 core LLVM function-data mismatch
warnings remain. The four host file-watch-limit failures in the full L07 core
run remain an outstanding workspace gate.

Native parent/updated debug runs on identical five-episode fixtures measured
three complete scans each: median **333 → 390 ms**, ranges **316–376 → 342–396**.
These small shared-host samples include the required additional name updates;
they are observations, not production performance estimates.

Evidence: `/tmp/ferrofin-dashboard-l09-checks.json`,
`/tmp/ferrofin-dashboard-l09-coverage.json`, `/tmp/ferrofin-dashboard-l09-native.log`,
and `target/dashboard-test-tmp/ferrofin-dashboard-l09-{ferrofin-zp68uhfy,jellyfin-kp4x_3cc}/results.json`.
