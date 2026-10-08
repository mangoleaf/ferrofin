# L13: image provider selection and order

The first implementation step makes automatic refresh use the registered
built-in remote image providers enabled for the item's type, independently of
the metadata provider selection. Path-less refresh previously fetched only TMDB artwork;
its shared image lookup now includes FanArt, AudioDB, OMDb, Studio Images and
TVDB where their providers support the item type. The first successful image
of each singular type wins instead of being overwritten by later providers.

The scanner now resolves TVDB series/episode artwork and OMDb posters when the
metadata provider did not run. TVDB season artwork uses the selected display
order with official-order fallback for an empty result. TMDB seasons use their
image listing. Episode TVDB artwork resolves through the parent series and
numbering, including absolute-order specials; an episode's own TVDB id does
not override that lookup. Existing metadata stills are reused only for the
same ordering.

Manual Choose Image deliberately includes disabled providers, matching
Jellyfin's controller. The automatic lookup honors the enable list; the manual
listing retains configured provider order. TVDB series, season and episode
providers now appear in that dialog. Its language tags use the localization
culture table and the TVDB-specific Chinese/Portuguese mappings.

Sources: Jellyfin `4910aafa1a`, `ProviderManager.GetImageProvidersInternal`,
`CanRefreshImages`, `GetAvailableRemoteImages`, `ItemImageProvider`, and
`RemoteImageController`; TVDB plugin `5c4592f`, the three image providers and
`TvdbSdkExtensions`. The season reference-type response is confirmed against
[TVDB's v4 schema](https://raw.githubusercontent.com/thetvdb/v4-api/master/docs/swagger.yml).

L13 remains **in progress**: final source checks found that default orders
must depend on the item type (TMDB movie 0/series 2, FanArt movie/series 1),
and embedded/WASM providers currently run in fixed stages. They must join the
same configured ordering before this finding is complete. Path-less refresh
also needs the registered WASM image implementations attached.

The remote-provider step passes all 789 provider tests, 468 focused core tests,
the real-server HTTP regression, formatting, SQL boundary, build and strict
workspace Clippy. The HTTP test saves TVDB selection with metadata disabled,
checks manual listing while automatic providers are disabled, then downloads
and serves the expected bytes for physical and path-less seasons. Native
five-phase scans on identical disposable movie fixtures measured a median
388 ms before (355–464) and 359 ms after (338–452); this is a baseline scan
regression check with remote fetchers disabled, not remote-download latency.
Separate line coverage passes: providers **93.84%** (21,813/23,245), core **94.72%** (103,162/108,916). Core combines the L11 baseline with all 468 current instrumented tests and 22 current binaries; the LLVM function-data mismatch warnings remain documented in the logs. Acquisition quantities, minimum widths and file
placement remain separate findings L14/L17. Unregistered upstream providers
and metadata-only display-order gaps remain open source findings; this record
does not certify those implementations.

Evidence: `/tmp/ferrofin-dashboard-l13-{checks,coverage,native}.json`, the matching logs, and `apps/ferrofin-server/tests/dashboard_metadata_locale.rs`.
