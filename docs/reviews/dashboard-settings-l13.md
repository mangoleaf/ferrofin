# L13: image provider selection and order

Automatic image refresh now uses the providers enabled for the item's type,
independently of metadata selection. Remote, named WASM and embedded audio
providers participate in the same configured ordering. Default order depends
on item type: for example TMDB precedes FanArt for movies, while FanArt precedes
TMDB for series. Later providers fill image types the earlier ones did not
supply. This applies to the filesystem scan, music post-pass and refresh of
items without a media path. The server now attaches the registered WASM image
implementations to that last path too.

The scanner resolves TVDB series/episode artwork and OMDb posters when the
metadata provider did not run. TVDB season artwork uses the selected display
order with official-order fallback for an empty result. TMDB seasons use their
image listing. Episode TVDB artwork resolves through the parent series and
numbering, including absolute-order specials; an episode's own TVDB id does
not override that lookup. Existing metadata stills are reused only for the
same ordering.

Manual Choose Image deliberately includes disabled providers, matching
Jellyfin's controller, and retains configured provider order. TVDB series,
season and episode providers now appear in that dialog. Its language tags use
the localization culture table and TVDB's Chinese/Portuguese mappings. Dynamic
WASM providers return bytes during acquisition and are not remote URL choices.

Sources: Jellyfin `4910aafa1a`, `ProviderManager.GetImageProvidersInternal`,
`CanRefreshImages`, `GetAvailableRemoteImages`, `ItemImageProvider`,
`RemoteImageController`, and the individual image-provider order declarations;
TVDB plugin `5c4592f`, its image providers and `TvdbSdkExtensions`; FanArt plugin
`7c2f8599`, its movie, series, season, artist and album image providers.
The TVDB season reference type is also confirmed against
[TVDB's v4 schema](https://raw.githubusercontent.com/thetvdb/v4-api/master/docs/swagger.yml).

Validation passes: all **805 provider tests**, **479 focused core tests**, the
real-server HTTP regression, formatting, SQL boundary, build and strict
workspace Clippy. Tests exercise plugin-first, remote-first, disabled and
unspecified order in both item and music passes, audio extraction ordering,
and path-less plugin refresh. HTTP saves TVDB selection with metadata disabled,
checks manual listing while automatic providers are disabled, then downloads
and serves the expected bytes for physical and path-less seasons.

Native five-phase scans on identical disposable movie fixtures measured a
median **361 ms before** (351–482) and **357 ms after** (343–483), comparing
`29ee460d` with the completed ordering change. Both return identical collection
results. This is a baseline scan regression check with remote fetchers disabled,
not remote-download latency; Docker remains unavailable. The first remote
provider implementation step separately measured 388 ms before / 359 ms after.

Separate line coverage: providers **93.79%** (21,947/23,400); core **94.71%** (103,304/109,071).
The core run combines its L11 whole-crate baseline with current instrumented
focused tests and current binaries. LLVM function-data mismatch warnings remain
in the evidence logs; this is accumulated coverage, not a new full core run.

Image quantities, minimum widths and file placement remain findings L14/L17.
The living document explicitly keeps S06–S08 open: TVDB metadata numbering,
the unregistered FanArt season provider, and video embedded-image extraction.
L13 completes the registered-provider selection and ordering repair; it does
not certify those missing provider implementations.

Evidence: `/tmp/ferrofin-dashboard-l13-{checks,coverage}.json`,
`/tmp/ferrofin-dashboard-l13-ordering-native.json`, matching logs, and
`apps/ferrofin-server/tests/dashboard_metadata_locale.rs`. The first remote
provider step's reports are preserved as `l13-remote-step-{checks,coverage,native}.json`.
