# L15: local metadata reader order

The book library editor now exposes Comic Provider, EPUB Metadata and Open
Packaging Format. Their saved `LocalMetadataReaderOrder` controls which source
wins when a book has competing metadata. The first successful local reader
supplies the metadata; subsequent local readers do not fill individual fields.
An empty or unsuccessful preferred reader falls through to the next one.

An omitted library order inherits the server's per-type order. An explicit
empty array restores registration order. Names match exactly, as upstream's
`Array.IndexOf` does. The existing disabled-reader list is also applied to the
book readers. Nfo remains the only registered local reader for the supported
movie/TV/music paths, so rearranging a singleton has no behavioral effect.

ComicProvider's internal comment/sidecar/archive order remains fixed. Its
external ComicInfo reader is now considered for every Book, including EPUB and
PDF, matching upstream. Book covers remain a separate open execution issue in
S09; local metadata ordering does not close image-provider ordering.

Sources: Jellyfin `4910aafa1a`, `ProviderManager.GetOrCreateOrderedProviders`,
`GetConfiguredOrder`, `MetadataService.ExecuteMetadataProviders`, ComicProvider,
ExternalComicInfoProvider, EpubProvider and OpfProvider. A disposable native
Jellyfin **12.1.0** run corroborates six precedence cases; the source pin remains
12.0-rc7.

Validation passes: **823 provider tests**, **488 focused core tests**, real HTTP,
formatting, SQL boundary, server build and strict workspace Clippy. The HTTP
regression saves reader order, scans conflicting local files, changes precedence,
and checks omitted versus explicit empty library order without restarting.
A separate valid EPUB fixture checks all three readers, case-sensitive ranking
and server fallback against native Jellyfin: all six final names match. Before
the change Ferrofin chose embedded EPUB metadata in all six cases.

Coverage and native timing details are recorded in
`/tmp/ferrofin-dashboard-l15-{checks,coverage,native}.json` and matching logs.

Separate line coverage passes: providers **93.64%** (22,353/23,871), core
**94.70%** (103,638/109,442). The core result combines the L14 profile with current
focused tests and 22 current binaries; LLVM reports 41 function-data mismatches.
All instrumented tests pass. This is accumulated coverage.

Native scan medians were **167 ms before** (156–173) and **184 ms after**
(161–244) over the initial scan and six refreshes. Refresh-only medians were
164 / 182 ms. These small, unisolated runs overlap other host work and make no
performance-improvement claim; the reported request-and-wait durations in the
raw fixture include a fixed one-second wait and are not API latency. Docker
was unavailable.
