# S18: repair the complete scan-change fixture

The complete scan matrix passes again without changing production behavior.
Its mock now provides separate TMDB image-list responses, including dimensions,
which the L14 acquisition policy requires. Movie default ordering expects TMDB
before FanArt, and the explicit-order case reverses that order and checks the
FanArt download. TVDB extended replies include actual English translations,
required when the plugin's original-language fallback is disabled.

Watcher create/delete rows retain exact probe, provider, table and item-write
assertions. The existing sibling episode remains unchanged: pinned EpisodeResolver
and BaseVideoResolver do not set IsInMixedFolder when a sibling appears. The
season refresh, series newest-media date, and created/deleted episode writes
remain required. Movie resolution has its own mixed-folder rules.

Source: Jellyfin `4910aafa1a`, EpisodeResolver, BaseVideoResolver and BaseItem;
TVDB plugin `5c4592f`, TvdbSeriesProvider, TvdbEpisodeProvider, TvdbSdkExtensions
and original-language fallback configuration; provider intrinsic ordering
already verified and recorded in L13. The expanded movie mock is extracted
into a helper to retain strict lint compliance.

Validation: both scan_change_detection tests pass, including the entire matrix
through its later provider, subtitle and outage scenarios. Formatting and
strict workspace all-target/all-feature Clippy pass. Final matrix run takes
25.43 seconds including the test command. This is a test-only server-crate
change, so it does not change production coverage or need a runtime performance
comparison. Evidence: `/tmp/ferrofin-dashboard-s18-checks.json` and corresponding
logs. This closes the fixture repair, not the separate final full workspace gate.
