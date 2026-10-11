# S07: the fanart.tv season image provider

The fanart.tv plugin ships a `SeasonProvider` (Backdrop, Thumb, Banner,
Primary). Ferrofin registered fanart.tv for movies and series only, so a
season's **Image fetchers** list never offered it, the season "Choose image"
dialog never listed it, and a scan never asked fanart.tv for season artwork.

Source oracle: `jellyfin-plugin-fanart` `7c2f859`,
`Providers/SeasonProvider.cs` (`Supports`, `GetSupportedImages`, `GetImages`,
`AddImages`, `PopulateImages`) and `Configuration/Constants.cs`.

## Behavior

- **Registered for `Season`.** fanart.tv's library registry entry covers
  seasons with the plugin's list `Backdrop, Thumb, Banner, Primary`, so the
  dashboard advertises it, the per-library checkbox and order apply, and its
  default order is the plugin's `Order = 1`.
- **`FanartClient::season_images(series_tvdb_id, season)`** reads the series'
  fanart document and keeps, from `seasonposter` (Primary, 1000×1426),
  `seasonbanner` (Banner, 1000×185), `seasonthumb` (Thumb, 1000×562) and
  `showbackground` (Backdrop, 1920×1080), only images whose `season` parses to
  the season's number — so an `all`-seasons background is never a season's.
  A thumb added before 2016-01-08, or with no usable date, is 500×281
  (`WorkingThumbImageDimensions`). Languages `00` become none, `http://` urls
  become `https://`, likes are the rating, and the list is ordered like the
  other fanart providers (widest, then language, then likes).
- **Where it runs.** The scan's season artwork pass, in the library's configured
  image order beside TheMovieDb and TheTVDB; and the manual "Choose image" /
  path-less refresh listing (`RemoteImageSource::FanartSeason`).
- **Accepted divergence — which id keys the document.** The plugin looks the
  document up by the *season's own* `Tmdb`/`Tvdb` provider id. A season's ids
  identify the season (TheTVDB's season id, TheMovieDb's season id), not a
  fanart.tv show, so upstream's lookup finds nothing outside coincidences.
  Ferrofin keys the document by the **series'** TVDB id — the document the
  season filter was written for — which is what makes the provider usable.

## Verification

- Unit (`fanart`): the season filter (the number, its zero-padded twin, not
  `all`, not blank, not other seasons), the array sizes, the thumb date rule at
  its boundary and for unusable dates, language `00`, rating, the case-
  insensitive `https` upgrade; the mock-server leg returns season 2's four images
  widest first and nothing for another season.
- Provider manager: a season under a series with a `Tvdb` id lists FanArt with
  `Backdrop, Thumb, Banner, Primary` and returns only that season's images.
- Scan (`library_scan_seasons`): with FanArt enabled for seasons the series'
  fanart document is read once more, for the season, by the series' TVDB id;
  unticked for the Season type, that request disappears and the series' own is
  unchanged.
- Production binary, previous baseline vs updated (offline fixture):

  | | Season image fetchers (library options) | Season remote image providers |
  |---|---|---|
  | before | TheMovieDb, TheTVDB | TheMovieDb, TheTVDB |
  | after | TheMovieDb, TheTVDB, **FanArt** | TheMovieDb, **FanArt**, TheTVDB |

  fanart.tv itself is not reachable from the fixture; its responses are covered
  by the mock-server tests above.

## Limitations

- A season's downloaded image still obeys the library's image limits; the
  default Season options keep only one Primary, so fanart's banner/thumb/backdrop
  appear once an administrator raises those limits.
- The scan's download of a season image goes through the shared acquirer; its
  `https` upgrade means a plain-HTTP mock cannot serve it, so image bytes are
  not asserted end to end here.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on providers passed: providers 915 (4 existing skips), core
2,448, extensions 96, livetv 299, wasm 62, server 235. Real FFmpeg tests
enabled; the WASM guest build remains disabled. The first run failed one
providers test that listed a season's advertised image types (now including
fanart.tv's Thumb; updated) and Clippy flagged `fetch_tv_still_images` at 101
lines, so the season arm moved into its own method; providers, the core
season/artwork tests and Clippy were rerun clean afterwards, while the other
package results above were taken before that refactor. Line coverage over each
crate's own sources: **providers 91.48%**, **core 95.11%**, both above the 80%
gate; traits and server composition are coverage-exempt. Instrumented builds
were cleaned right after their numbers were taken, leaving `target` at about
24 GiB with about 494 GiB free.
