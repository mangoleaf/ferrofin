# L19: allowed embedded subtitles

The saved `AllowEmbeddedSubtitles` mode now filters embedded video subtitles on
the next successful probe. AllowText retains text, AllowImage retains image,
AllowNone removes both, and AllowAll or an unknown enum value retains both.
External subtitles, other stream types and audio probing remain unchanged.
Filtering preserves the already assigned stream indexes, including gaps, and
a successful probe that filters every stream clears the previous stored rows.
An absent or failed probe preserves them.

Automatic scan downloads use the raw embedded streams before this display
filter, matching Jellyfin's ordering. Hidden text still satisfies a language;
hidden image subtitles satisfy it only when the embedded-skip option is set.
External streams are read afresh for each language so subtitles attached since
the probe began, including earlier downloads, are not lost or downloaded twice.

Source oracle: Jellyfin `4910aafa1a`,
`MediaBrowser.Providers/MediaInfo/FFProbeVideoInfo.cs`, `Fetch` and
`AddExternalSubtitlesAsync`; filtering follows external merging and numbering.
The existing MediaStream classifier supplies text/image semantics.

Validation: all **348 focused core tests** pass instrumented. The initial normal
run passed 347 and found an invalid parent foreign key in the new SQLite fixture;
that fixture was corrected and its regression passes normally. Real-server HTTP
checks all five saved mode transitions with text/image/external subtitles.
Formatting, SQL boundary, server build and strict workspace Clippy pass.
Separate core line coverage is **94.90%**, using the fresh full-core baseline
plus current focused tests, with no LLVM export warnings. The normalizer drops
codec-less raw probe streams; the direct core cases additionally test their
classification when supplied in MediaInfo.

Native HTTP uses a tiny real MKV containing English text and an external English
SRT. After-change stream types, codecs, external flags and indexes match the
Jellyfin **12.1.0** reference in All/Text/Image/None/All. Before-change incorrectly
exposed the embedded text in every mode. Refresh completion is observed through
an advancing Etag. Before/after milliseconds for these five operations are
**237.96/246.61, 212.27/222.60, 218.62/225.22, 214.48/215.31,
218.27/241.20**. These unisolated local observations make no performance
improvement claim; Docker was unavailable for the isolated benchmark suite.

Evidence: `/tmp/ferrofin-dashboard-l19-{checks,coverage,native}.json`, native
before/after/reference JSON and corresponding logs. Builds remain serialized
with debug/incremental output disabled, pooled coverage profiles, a 30 GiB target
limit and a 512 GiB host reserve. Generated artifacts occupy about 10 GiB.
