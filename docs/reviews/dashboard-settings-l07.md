# L07: metadata language and country reach the providers

Library language and country now inherit from the item's nearest explicit
parent setting and then the live server defaults. Blank library preferences
inherit; changing the server setting takes effect on the next scan without a
restart. The scanner caches ancestor locale fields for that pass and keys
TMDB season responses by language, preventing one translated season response
from being reused for a different locale.

TMDB search, details, episodes, seasons and artwork receive the resolved
language. Movie and series ratings use the requested country's certification,
including Germany's FSK prefix, with the provider's US fallback. TVDB series
and episode names/overviews select translations and skip aliases; ratings use
the requested country's three-letter code. Fanart requests rank artwork using
the item's resolved language. Music metadata/artwork resolve ancestor settings
too. The pathless season/episode refresh shares the resolved TMDB locale.

Sources: Jellyfin `4910aafa1a`, `BaseItem.GetPreferredMetadataLanguage` and
`GetPreferredMetadataCountryCode`, and the TMDB providers; TVDB plugin
`5c4592f`, translated-name/overview selection and country ratings; fanart
`GetImages` ordering. TVDB uses the plugin's default empty fallback-language
list and disabled original-language fallback. Configuring those plugin-specific
options remains PL05; completing L07 does not certify the separate server
settings and by-name-item paths in M01/M02.

Validation: 458 focused core/provider tests, the new native HTTP integration
test, SQL boundary, formatting, server build and strict workspace Clippy pass.
The HTTP test scans a pinned movie against a recording local provider:
server fr/FR produces French metadata and FR rating, library de/DE overrides
both, then clearing the library preferences and saving server es-419/AR
produces the normalized es-AR request and AR rating without restarting.
Mock provider regressions cover TVDB translation/country selection, country
rating fallback, ancestor precedence, language-separated season caches and
fanart language/width/rating priority and request isolation.

Fresh separate coverage meets both thresholds: core **94.99%**
(102,605 / 108,018 lines, 22 current binaries) and providers **93.71%**
(21,370 / 22,804 lines, four current binaries). The full core run passed
2,169 of 2,173 tests; four existing `notify_watcher` tests fail because the
host reports its OS file-watch limit. All 760 provider tests pass (four skips).
LLVM reports 19 core function-data mismatches; this is not a clean workspace
gate while the host watcher failures remain.

Measured native debug scans on the same two-episode fixture: parent median
**307 ms** (261–357), final build **353 ms** (281–409), three scans each.
Providers were enabled but outbound requests failed quickly through a local
closed proxy, exercising locale/ancestor lookup without external network
variance. These shared-host observations do not measure successful provider
latency; no production performance claim is made. The HTTP integration test
separately verifies successful provider responses.

Evidence: `/tmp/ferrofin-dashboard-l07-checks.json`,
`/tmp/ferrofin-dashboard-l07-coverage.json`,
`/tmp/ferrofin-dashboard-l07-timing-ferrofin-w2jnr0xe/server.log`, and
`target/dashboard-test-tmp/ferrofin-dashboard-l07-timing-ferrofin-3odltk_o/server.log`.

