# L18: similar-item provider selection and order

Verified complete. The shared owning-library lookup introduced in L01 already
corrected the original physical-location lookup finding; this change adds
regressions without changing production similarity logic.

A real virtual-folder manager and SQLite store verify an adopted-style top
parent distinct from the collection folder, path fallback, live saved options,
case-insensitive enable/order names, explicit order, selection-order fallback
and clearing remote selection without restarting. Local similarity remains
available regardless of the remote selection, matching the pinned source.

The real HTTP scenario creates a second movie with a distinct TMDB ID. An
unchecked provider makes no remote request; enabling TMDB places its referenced
movie first; disabling it stops further remote/cache execution. Existing tests
cover local-versus-remote ordering, unchecking the local entry, cache behavior,
duplicates, visibility and provider failures.

Source: Jellyfin `4910aafa1a`,
`Emby.Server.Implementations/Library/SimilarItems/SimilarItemsManager.cs`,
`GetSimilarItemsAsync` and `GetConfiguredSimilarProviderOrder`. Remote providers
are opt-in, local providers always participate, ranking ignores case and an
empty explicit order falls back to the enabled-provider list.

Validation: 34 similarity tests pass normally and instrumented; the extended
real-server HTTP test passes; formatting and strict workspace Clippy pass.
Separate core line coverage is **95.14%**, using the fresh L17 full-core baseline
plus these current tests, with no LLVM export warnings.

Native disposable HTTP fixtures exercise saved disabled/enabled/disabled
selection on a movie without a remote provider ID. Fourteen settled requests
per phase measure median milliseconds before/after: **3.48/3.43**, **2.67/2.76**,
**2.70/4.18**. All responses contain the expected empty result. These unisolated
observations make no performance improvement claim; production logic did not
change and Docker was unavailable for `bench/`. The mock-backed HTTP scenario,
rather than this no-ID timing fixture, verifies remote result selection.

Evidence: `/tmp/ferrofin-dashboard-l18-{checks,coverage,native}.json`,
`native-{before,after}.json` and the corresponding logs. Builds remain serialized
with debug/incremental output disabled and the 30 GiB/512 GiB storage budgets.
