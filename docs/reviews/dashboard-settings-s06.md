# S06: TVDB metadata follows the series display order

The filesystem metadata pass asked TheTVDB for every season and episode in
`official` order. Both lookups now use the series' stored `DisplayOrder`, as
the TVDB plugin does, and the series' order is now read from its NFO and
shown to clients.

Source oracle: Jellyfin `4910aafa1a` and the TVDB plugin `5c4592f` —
`TvdbSeasonProvider.GetMetadata`, `TvdbClientManager.GetEpisodeTvdbId`,
`TvdbEpisodeProvider.MapEpisodeToResult`, `Season.cs:268` / `Episode.cs:314`
(`SeriesDisplayOrder = series.DisplayOrder`),
`LibraryManager.FillMissingEpisodeNumbersFromPath`, `BaseNfoParser`
(`<displayorder>`), `MetadataService.MergeDisplayOrder` and
`DtoService.cs:1207`.

## Behavior

- **Seasons.** The season id is read from the series' season list for the
  display order — `official` when it is blank — matched case-insensitively with
  no fallback to another order when the series lists no such season. Scan and
  path-less refresh both read the series row.
- **Episodes.** The ordering, season and number asked for follow the plugin's
  case-sensitive switch (`tvdb::episode_lookup`): `regional`, `alternate`,
  `altdvd`, `dvd` and `alttwo` use their own order with the episode's season;
  `absolute` uses the `absolute` order in season 1, except specials (season 0),
  which use `official`; anything else is `official`. A season-less episode is in
  season 1 (`ParentIndexNumber ?? 1`); previously it was not looked up at all.
- **Aired-order fields.** `AirsBeforeEpisodeNumber`, `AirsAfterSeasonNumber`
  and `AirsBeforeSeasonNumber` are taken from TVDB only when the series has no
  display order ("only applicable for Aired Order").
- **Episode stills.** The metadata lookup and the image provider now use the
  same ordering, so a still found by the metadata pass is reused for every
  order, not only `official`.
- **Absolute naming.** A series whose display order is `absolute` (ordinal,
  ignore case) resolves its episodes' numbers with `isAbsoluteNaming`: only the
  absolute-number expressions run, so a name they do not match (`Show.102`)
  takes nothing from the path instead of reading season 1, episode 2.
- **`<displayorder>` is applied.** The NFO parser read it, but the scan never
  stored it, so no series could carry one from its NFO. It is now filled into
  the series row's `Data` blob (the merge rule `MergeDisplayOrder` already
  existed there), and the NFO saver writes it back.
- **DTO.** Series and BoxSet DTOs now emit `DisplayOrder` (previously never
  emitted, so the web editor's "Display order" select was always blank).
- Each pass reads a series' order once and shares it between its seasons and
  episodes.

## Verification

- Unit: the episode lookup for every order (case-sensitive, specials,
  season-less), the raw reader and the season default, the DTO field for a
  series, a blank value, a collection and a movie, and absolute naming on a
  stored episode row.
- Scan (`library_scan_seasons`): a series whose `tvshow.nfo` says `dvd` reads
  its season from the `dvd` season TVDB lists (76), makes no request for the
  official season (77), and asks `/episodes/dvd`, never `/episodes/official`.
- Production binary, previous baseline vs updated, three series with
  `tvshow.nfo` orders `dvd` and `absolute` and none:

  | | Series DTO `DisplayOrder` |
  |---|---|
  | before | none, none, none |
  | after | **`dvd`, `absolute`**, none |

  The binary has no TVDB base-URL override and the fixture has no network, so
  the TVDB requests themselves are covered by the scan test above, not by the
  binary. Episode numbers were identical before and after (1×2): a new episode
  is numbered from its file name by the planner, and the absolute filter only
  matters for a stored row with missing numbers.

## Limitations

- The planner still numbers a new loose episode without the absolute filter;
  upstream applies it only in `FillMissingEpisodeNumbersFromPath`, as here.
- TVDB's date-based episode lookup (`PremiereDate` without a number) is not
  ported in the scan's episode arm; it was not before.
- The path-less episode refresh still runs only TheMovieDb (S05).

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on a changed crate passed: providers 911 (4 existing
skips), core 2,445, extensions 96, livetv 296, wasm 62, server 230. Real
FFmpeg tests enabled; the WASM guest build remains disabled. The first Clippy
run flagged `fetch_season_providers` at 102 lines; the series-order read moved
into a helper and Clippy and the season tests were rerun clean, while the
package and coverage results above were taken just before that refactor. Line
coverage over each crate's own sources: **providers 91.40%**, **core 95.09%**,
both above the 80% gate; traits and server composition are coverage-exempt.
The instrumented core build took `target` to 42.9 GiB, above the 30 GiB cap;
each coverage cache was removed right after its numbers were taken, leaving
24.2 GiB with about 494 GiB free.
