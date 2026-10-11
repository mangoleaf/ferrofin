# S32: home-video libraries resolve plain videos

A library registered as **Home Videos** resolved its files as `Movie`s, named
and dated by the movie parser. Jellyfin's `MovieResolver` resolves them as
generic `Video`s with `parseName: false` and treats a folder of videos as just
a folder; the item type is visible to every client (`Type`), and the choice
decides names, year, `IsMovie` and which movie-only behaviors an item takes
part in.

Source oracle: Jellyfin `4910aafa1a`, `MovieResolver` (`Resolve`:
`ResolveVideo<Video>(args, false)` for `homevideos`/`photos`; `FindMovie`'s
`isPhotosCollection` arm; `ResolveVideos<Video>(…, supportMultiEditions: false,
…, parseName: false)`), `VideoResolver` (`parseName: false` keeps the stem and
no year); evidence note `brain/knowledge/DASHBOARD_SETTINGS_S32_HOME_VIDEO_
RESOLVER.md`.

## Behavior

- A home-video library plans every video file as a `Video` (stored type
  `MediaBrowser.Controller.Entities.Video`): `Name` is the file stem as written
  (`Beach Day (2021)`), `ProductionYear` is not read from it, `IsMovie` is
  false, `IsInMixedFolder` is true (also for a lone file in its own folder),
  `MediaType` is `Video`. A disc rip (`VIDEO_TS`, `BDMV`) still resolves as one
  item for its folder, now a `Video`.
- A folder of videos is no title's "own-folder movie" there: `FindMovie` only
  makes one outside photo collections, so the single-video-folder naming and
  alternate-version grouping do not apply.
- Movie and music-video libraries are untouched (`Movie` with the folder name
  rule; `MusicVideo` named by its stem); the `null`/mixed library arms are not
  changed here.
- **Existing rows keep their identity.** A file an earlier build stored as a
  `Movie` is planned at the same path as a `Video`: the scan reuses that row's
  id (so user data, links and history stay attached) and moves its type to
  `Video`; no second row appears. An adopted `Video` row is reused as before.
  Rows keep their stored name until a metadata refresh replaces it.
- Photo handling (`EnablePhotos`) is a separate consumer and unchanged.

## Verification

- Scan tests: a flat file, a file in a folder, and a lone file in its own
  folder become `Video`s named by stem with no year, `IsMovie` false, mixed
  folder, and no `Movie` row exists for them; a movies library still makes the
  `Heat (1995)` movie from its folder and a music-video library the stem-named
  `MusicVideo`; a pre-existing `Movie` row at the path survives two scans as one
  row of type `Video` with its id; the photo-toggle test (which seeds home
  videos) now follows the `Video` identity.
- Production binary, previous baseline then updated on the same data directory,
  two home videos, one marked played:

  | | Type | Id | Played |
  |---|---|---|---|
  | before (old binary) | `Movie`, `Movie` | `70e8cb58…`, `cf925f6e…` | yes, no |
  | after (new binary, rescans) | **`Video`, `Video`** | **unchanged** | **yes, no (kept)** |

## Limitations

- Stacked-part grouping (`cd1`/`cd2`) of flat files is not applied to home
  videos, as it is not to movie files in a flat folder here.
- The `null`/mixed-library resolver arms and generic `Video` local readers
  (S10) are separate.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on core passed: core 2,458, extensions 96, livetv 299, wasm
62, server 237. Real FFmpeg tests enabled; the WASM guest build remains
disabled. Line coverage over core's own sources: **95.13%**, above the 80%
gate; traits and server composition are coverage-exempt. The instrumented
build was cleaned right after the numbers were taken, leaving `target` at
about 24 GiB with about 494 GiB free.
