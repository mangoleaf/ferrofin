# L25: configured embedded music tags

Audio probes retain an internal, serde/schema-skipped snapshot of unsplit tags.
The scanner interprets it using the owning library's live options instead of
accepting the normalizer's unconditional splitting. Defaults use standard
ARTIST/ALBUMARTIST fields; PreferNonstandardArtistsTag selects ARTISTS and
ALBUMARTISTS when supplied. Custom separators apply only when enabled, accept
one UTF-16 character each, and retain the configured whitelist spelling before
remaining split names. NUL sanitation, actual U+001F multivalues, per-value
distinct handling and MusicBrainz field/alias priority follow the pinned source.

Existing field locks and metadata merge rules are retained. As in upstream,
changing the option does not rewrite already populated artist metadata merely
on Save; replacement refresh is used to reinterpret those fields. Encoders that
supply already interpreted MediaInfo without raw tags retain their seam contract.

Source oracle: Jellyfin `4910aafa1a`,
MediaBrowser.Providers/MediaInfo/AudioFileProber.cs,
MediaBrowser.Model/Extensions/LibraryOptionsExtension.cs and NameExtensions.cs.

Validation: all **961 model**, **919 mediaencoding**, and **76 core/music**
tests pass. Regressions cover raw-tag wire exclusion, normalization precedence,
preferred/missing/empty/NUL tags, Unicode whitespace/case/diacritics, whitelist
order, multivalues, valid provider IDs and repeated live scanner policies.
Real HTTP performs six saved-option replacement refreshes and verifies actual
item fields. The metadata HTTP regression, full scan matrix, SQL boundary,
formatting, build and strict workspace Clippy pass. The prepared HTTP fixture
was adapted to L24's new LrcLib test endpoint; an empty-string test construction
was corrected to meet the strict lint gate.

Native before/after/reference uses real FLAC tags in seven saved configurations.
Before-change matches none; after-change matches all seven native Jellyfin
12.1.0 observations, including provider-ID retention and AlbumArtist. The source
pin and native runtime are distinct. The fixture separately settles the library's
named-artist catalog after track refresh: immediate reference AlbumArtists pairs
can be absent before that pass. Its time is excluded from track-refresh timings;
S19 records the separate DTO identity/mapping gap. Public providers are blocked
through the local proxy, including reference by-name provider attempts.

Before/after refresh response ranges are **1.94–3.96/2.05–2.91 ms**; observed
time until Etag changes is **236.59–244.42/229.34–237.47 ms**, including 50 ms
polling. These single unisolated observations make no improvement claim.
Docker is unavailable. Independent model coverage is **90.16%** (8,856/9,822), and mediaencoding
coverage is **90.92%** (17,004/18,702), from their full instrumented suites.
Core coverage is **85.89%** (95,671/111,391), with all 76 affected tests
rerun instrumented and unchanged coverage seeded from L24. Model/core exports
emit no warnings; the fresh mediaencoding export reports nine functions with
mismatched profile data. Each changed crate independently passes 80%.

Audio credit persistence remains S17. The real fixture verifies ordinary FLAC
tag strings; it does not establish ATL-equivalent repeated-container value
boundaries for every format. Actual U+001F values are guarded by resolver tests.

Evidence: `/tmp/ferrofin-dashboard-l25-{checks,coverage,native}.json` and matching
logs/exports. Builds remain serialized under the 30 GiB target cap and 512 GiB
host reserve; current generated artifacts are approximately 12 GiB.
