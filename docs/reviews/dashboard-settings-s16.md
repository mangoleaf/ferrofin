# S16: saved item metadata in subtitle searches

Subtitle searches now forward the item's complete saved ProviderIds map,
episode IndexNumberEnd and runtime to registered providers. The existing
OpenSubtitles REST adapter also receives the saved IMDb ID, resolving its key
case-insensitively. Saved metadata replaces stale caller values; absent IDs,
absent/invalid ranges and non-episodic items clear the corresponding values.
IDs come from the existing database repository read, with no new manager SQL.

The source oracle is Jellyfin `4910aafa1a`,
`MediaBrowser.Providers/Subtitles/SubtitleManager.cs`, the overload creating
a SubtitleSearchRequest from Video and Episode. A .NET harness calls the
local Jellyfin 12.1.0 managed manager with a capturing provider. It verifies
all three ID keys/values, episodes 2–3, season 1, runtime 42,000,000 and an
unchanged perfect-match flag, plus a movie without episode range/runtime.
The runtime constructor includes its additional directory-service argument;
that does not change the request mapping under review. Sources, failed harness
build attempts and corrected observed output remain under
`/tmp/ferrofin-followup-two/subtitle-oracle/` and `reference-proof.json`.

The real Rust manager regression seeds the actual store, captures the provider
request, checks the same range/IDs/runtime, preserves a custom provider key,
clears stale metadata, handles malformed/overflowing range data and excludes
movie ranges. The provider's non-hash candidate must remain rejected when
perfect matching is enabled. The real HTTP integration fixture then checks
that saved IMDb metadata reaches the OpenSubtitles request with both perfect
matching enabled and disabled, while existing download behavior still works.
The built-in REST provider's query continues to use its existing single episode
parameter; providers receive the full range through the shared request.
S15's separate provider namespace contract remains open.

A disposable production-binary fixture measures ten subtitle-search requests
before and after the change, with a real scanned video and saved IDs. Its
provider is unconfigured and returns no candidates: these samples measure the
item/search path, not a provider response or download. Provider mapping is
verified independently by the capturing-manager and offline HTTP tests.
All owned native fixture servers are reaped. Shared-host request timings are
observations, not publishable benchmark results.

Formatting, strict workspace Clippy, workspace doctests, the SQL boundary,
production build, targeted regressions, the real HTTP integration fixture and
native checks passed. The source-qualified workspace matrix passed **8,411
tests** across 21 packages and 201 default test targets.
Changed packages and every dependent were rerun; source-identical independent
packages retain their prior passing results after source/dependency checks.
Real FFmpeg tests were enabled. Five existing DB/provider skips remain visible;
the WASM guest build was separately disabled explicitly.

Fresh unseeded **ferrofin-core line coverage is 95.40%**, above
its separate 80% gate. Traits and server wiring are coverage-exempt under
CLAUDE.md. These final gates cover S30 and S16 together. Tested source files,
embedded assets, final native binary and canonical profile hashes are checked
in `/tmp/ferrofin-followup-two/quality-proof.json`. Complete logs and profiles
remain under `/tmp/ferrofin-followup-two/`.

Median elapsed subtitle-search time across ten local requests: previous binary
3.361 ms, updated binary 2.341 ms.
The unconfigured-provider limitation above applies. Final localization request
median was 0.793 ms; complete catalogs match Jellyfin.

Only generated build artifacts were cleaned. The worktree, commits, production
binary, native datasets and canonical profiles remain intact. At this validation
checkpoint, target usage is **11.78 GiB**, with
**510.38 GiB free** on the host.
