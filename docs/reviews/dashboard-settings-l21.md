# L21: subtitle provider selection and order

Automatic subtitle searches now use an independent first-success flag. Manual
searches retain the source default of querying every provider concurrently and
returning candidates in selected order. Automation alone no longer changes the
search-all behavior. Provider errors and empty results allow ordered fallback.

Registered providers first sort by intrinsic order. Saved SubtitleFetcherOrder
uses exact display names; DisabledSubtitleFetchers remains case-insensitive.
Movie/Episode support filters both actual-item searches and dashboard provider
descriptors. Unsupported kinds and missing descriptor items return no providers;
actual library lookup errors propagate. Generic searches retain their requested
media type when no item resolves.

Source oracle: Jellyfin `4910aafa1a`,
`MediaBrowser.Providers/Subtitles/SubtitleManager.cs` and
`src/Jellyfin.Extensions/ReadOnlyListExtension.cs`. The interactive video overload
uses default provider selection independently of a library's automatic settings.
S15 records the existing readable provider-ID namespace difference from upstream
MD5 IDs; S16 records incomplete ProviderIds/episode-range enrichment. These gaps
remain open and are not counted as fixed by this setting.

Validation: **52 focused core tests** pass normally and instrumented, including
a concurrency barrier that requires simultaneous searches, exact saved casing,
intrinsic order, failure/empty fallback, all-disabled silence, media-type
filtering and independent automation/search-all flags. Real HTTP saves provider
settings, checks disabled/re-enabled automatic calls and preserves manual
selection. The independent scan/rescan/probe-failure test also passes. Formatting,
server build and strict workspace all-target/all-feature Clippy pass. Separate
core line coverage is **94.33%** (103,992/110,246), with no LLVM export warnings;
unchanged coverage is seeded from L20 and current affected tests are rerun.
The broader scan matrix remains the separately documented S18 validation repair.

Native before/after uses the eleven-phase mock-backed HTTP fixture from L20.
The before observations are reused from the exact L20 committed binary retained
as dashboard-l21-parent; after observations come from the rebuilt L21 binary.
All configured provider/language/skip outcomes pass, and manual search returns
one candidate on both builds. Disabled-provider refresh is **364.08/338.66 ms**,
re-enabled saved-order refresh **364.83/372.25 ms**, and manual search
**5.71/5.49 ms**, before/after respectively. Single-provider native observations
cannot measure multi-provider fan-out; the concurrency regression covers that
contract. These unisolated numbers make no performance improvement claim.
Docker is unavailable and the native Jellyfin package lacks OpenSubtitles, so
remote provider parity uses the pinned source and local provider mock.

Evidence: `/tmp/ferrofin-dashboard-l21-{checks,coverage,native}.json`, native
before/after JSON, and corresponding logs. Builds remain serialized, with no
incremental/debug artifacts, a 30 GiB target cap and 512 GiB free-space reserve.
