# S10: local NFO readers beyond movies, series and episodes

The scanner's NFO reader now covers the item kinds Jellyfin's NFO providers
cover and searches in their order.

Source oracle: Jellyfin `4910aafa1a`, `MediaBrowser.XbmcMetadata` —
`BaseVideoNfoProvider` (`VideoNfoProvider`, `MusicVideoNfoProvider`,
`MovieNfoProvider`: `MovieNfoSaver.GetMovieSavePaths` + `MovieNfoParser`),
`SeasonNfoProvider`, `SeriesNfoSeasonProvider` / `SeriesNfoSeasonParser`, and
`BaseNfoProvider` (`HasChanged`, `IndexNumber = info.IndexNumber`).

## Behavior

- **Video and MusicVideo are read.** The dispatch had no arm for them; both
  now use the movie parser, as upstream's providers do.
- **Movie-family search order** follows `GetMovieSavePaths`, which the
  scanner had reversed and simplified: a DVD's `VIDEO_TS/VIDEO_TS.nfo`;
  `movie.nfo` in the containing folder for a *Movie* that is not in a mixed
  folder (owned videos and music videos never read it); then the folder-named
  `.nfo` of a disc structure (not a placeholder), otherwise the file's own
  `.nfo`. Before, a movie file read its own `.nfo` first, so when both existed
  `movie.nfo` lost to `<name>.nfo`.
- **Seasons have a second reader.** After `season.nfo`,
  `SeriesNfoSeasonProvider` names the season from a matching
  `<namedseason number="N">` in `tvshow.nfo` in the season folder, else the
  series folder. The first sidecar that reads wins, so a readable `season.nfo`
  suppresses it. The reader needs the season's number, which upstream seeds on
  the item (`IndexNumber = info.IndexNumber`); the scan now seeds it for every
  kind, deriving a season's number from its folder when not yet set
  (`Season.BeforeMetadataRefresh`).
- **Change monitoring covers every group.** The planner's NFO sidecar fact is
  the newest first-existing file across a kind's provider groups, so a
  `tvshow.nfo` edit alone re-reads its seasons, subject to upstream's
  one-minute NFO save tolerance.

## Verification

- Unit: the candidate groups for movie, mixed-folder movie, owned video,
  music video, folder movie, DVD, Blu-ray, DVD placeholder, series, episode and
  season.
- Scan (`library_scan_extras`): `movie.nfo` beats `Heat.nfo`; a music video's
  sidecar supplies its title and overview; a season is named from `tvshow.nfo`
  and a readable `season.nfo` wins; editing `tvshow.nfo` renames the season on
  the next scan.
- Production binary, previous baseline vs updated (empty media files, one
  library per kind, NFO reader only):

  | | before | after |
  |---|---|---|
  | movie with `movie.nfo` and `Heat.nfo` | `From Heat.nfo` | `From movie.nfo` |
  | music video with `Song.nfo` | `Artist - Song (2001)` | `Local video title` |
  | season 1 with `<namedseason>` in `tvshow.nfo` | `Season 1` | `First Arc` |

  The scan adds a handful of `stat` calls per item; no timing was measured.

## Limitations

- The series folder for the season fallback is taken as the season folder's
  parent (upstream asks the library for the parent item's path); they agree
  for the folder layouts the scan resolves.
- `Video` items arise from home-video libraries, which Ferrofin still resolves
  as movies (S32); the Video reader is in place for when that is fixed.
- The upstream tie between the two season providers' registration order is not
  observable in the source; `season.nfo` is tried first.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on core passed: core 2,432, extensions 96, livetv 296,
wasm 62, server 230. Real FFmpeg tests enabled; the WASM guest build remains
disabled. The first Clippy run flagged the new 110-line candidate test
(`too_many_lines`); it now carries the allow the repository uses on long
tests, and Clippy was rerun clean afterwards (the test results above predate
only that attribute). Line coverage over core's own sources: **95.09%**, above
the 80% gate; traits and server composition are coverage-exempt. The
instrumented build took `target` to 42.4 GiB, above the 30 GiB cap; its cache
was removed right after the numbers were taken, leaving 23.9 GiB with about
494 GiB free.
