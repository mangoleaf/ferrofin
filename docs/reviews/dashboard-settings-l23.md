# L23: live lyric destinations

Lyric uploads and provider downloads now resolve the item's owning library
through the shared resolver. This fixes nested locations and adopted physical
parent IDs that previously selected the first matching library. Each save reads
the live SaveLyricsWithMedia option and metadata root. Enabled media storage
tries the media directory first and falls back internally if that write fails;
disabled storage uses the internal directory directly.

The original supported classification was correct for a single ordinary
library, but missed the ownership defect. Source oracle: Jellyfin
`4910aafa1a`, MediaBrowser.Providers/Lyric/LyricManager.cs, TrySaveLyric and
TrySaveToFiles; the latter explicitly retries the internal destination.

Validation: 24 focused core tests pass, including nested/adopted ownership,
off/on/off changes, live root replacement, selected-target failure and existing
parser/provider/task regressions. Real HTTP verifies saved library options,
uploaded lyric contents and exact files through all four destination phases.
SQL boundary, formatting, server build and strict workspace Clippy pass.

Native before/after/reference checks agree on destination files and read-back
text in all four phases. The reference is Jellyfin 12.1.0, distinct from the
source pin. Its upload queues metadata refresh, so the fixture waits for the
lyric stream to settle before checking read-back or deleting the next fixture.
The initial immediate-read reference attempt raced that queue; its replacement
passes. The HTTP upload response alone is timed, excluding settlement polling.

Before/after upload observations are 4.23/4.50 ms (internal), 8.93/5.58 ms
(media), 4.88/4.80 ms (new internal root), and 6.25/4.28 ms (blocked media with
fallback). These are single unisolated local observations, with no performance
improvement claim; Docker is unavailable.

Evidence: `/tmp/ferrofin-dashboard-l23-{checks,coverage,native}.json` and the
corresponding focused-test, native before/after/reference and coverage logs.
Core coverage is **94.37%** (104,437/110,668), with all 24 affected tests
rerun instrumented, unchanged coverage seeded from L22, and no LLVM export
warnings. Builds remain serialized under the 30 GiB target cap and 512 GiB host reserve.
