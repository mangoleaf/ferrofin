# L20: automatic subtitle languages and matching controls

Automatic downloads now distinguish matching text from image subtitles.
Matching text satisfies its language; a matching external image subtitle still
needs text, and the embedded-skip switch applies only to embedded subtitles.
The downloader retains default-audio matching, configured languages and perfect
hash matching, including live changes to each saved option.

Eligibility now excludes disc formats, channel livestreams and active DVR
captures. The DVR manager is held weakly to avoid a library ownership cycle.
The daily task resolves the owning library, including nested and adopted paths,
and applies upstream's language candidate preselection: any matching audio
track when audio skipping is enabled, and external/all matching subtitles
according to the embedded-skip flag. The downloader's final check still uses
default audio and text rules. The task configures SourceTypes=Library as in
upstream; neither repository currently consumes that query field, so this is
not a claim of an additional source-type filter.

Source oracle: Jellyfin `4910aafa1a`,
`MediaBrowser.Providers/MediaInfo/SubtitleDownloader.cs`,
`SubtitleScheduledTask.cs`, `MediaBrowser.Controller/Entities/Video.cs` and
`MediaBrowser.Model/Entities/MediaStream.cs`.

Validation: **48 focused core tests** pass normally and instrumented, including
real SQLite/provider downloads and live saved nested-library options. Real
HTTP checks required/relaxed perfect matching, both skip toggles against the
raw filtered probe snapshot, external VobSub acquiring text, disabled providers
and empty languages. The existing subtitle scan helpers run independently to check existing sidecars,
quiet rescans, provider outages and failed probes; core tests cover cancellation. Formatting,
SQL boundary, server build and strict workspace Clippy pass. Separate core
line coverage is **94.32%**, with no LLVM export warnings.

Native before/after HTTP uses a tiny real MKV with French audio and Spanish PGS,
an external Portuguese PGS, and a local mock provider behind a temporary TLS
CA trusted only by the server child. All eleven phases complete. Before-change
skips the needed Portuguese text; after-change downloads it and preserves the
image subtitle. Other matching/perfect/skip/language cases retain their expected
outcomes, and manual search retains upstream's independent provider defaults.
The first six comparable refreshes have median milliseconds **239.04 before /
243.36 after**, ranges **214.89–262.34 / 215.10–282.08**. The external-image case
changes the work performed, so its **336.01 / 323.71 ms** is not a like-for-like
latency comparison. These unisolated observations make no improvement claim.
Docker was unavailable; the native Jellyfin distribution lacks the OpenSubtitles
plugin, so remote provider parity uses pinned source and mock-backed HTTP.

Evidence: `/tmp/ferrofin-dashboard-l20-{checks,coverage,native}.json`, native
before/after JSON and corresponding test/native logs. The initial TLS fixture
incorrectly used its CA as a leaf; that invalid run was replaced with a proper
CA-signed leaf before recording these measurements. Build storage remains
bounded to 30 GiB with a 512 GiB host reserve; current target is about 10 GiB.

The broader pre-existing scan matrix is not green: it expects a metadata-detail
poster URL to be downloaded without providing the separate image-list response.
A temporary corrected image fixture then exposed its stale mixed-folder watcher
counts before reaching subtitles. Those exploratory changes were removed; the
independent subtitle test supplies its own probe-outage setup. Repair the full
image-list/mixed-folder matrix before claiming the next workspace gate passes.
