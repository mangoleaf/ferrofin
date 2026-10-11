# S21a: the user's saved view order (`OrderedViews`)

S21 collects several independent ordering gaps. This is the first slice that
closes — the one a user sees on the home screen: `OrderedViews`, the library
order a user saves in their display settings.

`GetUserViews` ends with the user's own order (`UserViewManager.cs:141-158`):
a view whose id is in `OrderedViews` takes that position, a user view that
stands for a library takes its `DisplayParentId`'s, every other view follows,
and ties keep the name order. Ferrofin kept the preference (it round-trips
through `/Users/Configuration`) but never applied it: the home screen and
`/UserViews` ignored the order a user saved.

## Behavior

- `FerrofinUserViewManager::get_user_views` (and the `IncludeHidden` form) stable
  sorts its views by the user's saved `OrderedViews`: a view by its own id, else
  by its `DisplayParentId` (read from the view's `Data`), else after every named
  view, all ties staying in the name order the list already has. Ids that name no
  view are ignored; views the user hid (`MyMediaExcludes`) are removed first, as
  upstream. With nothing saved nothing changes.

## Verification

- Core test: four libraries, no saved order (name order); `OrderedViews` naming
  two of them plus an id that is no view (the two first in that order, the rest by
  name, the stray id ignored; the `IncludeHidden` list ordered the same); then one
  of the named views hidden through `MyMediaExcludes` (gone, the rest still in
  order).
- Production binary, three libraries, then `POST /Users/Configuration` with
  `OrderedViews` = [Music, Movies], `GET /UserViews`:

  | | order |
  |---|---|
  | before | `B Shows, C Music, A Movies` (the order does not change) |
  | after | **`C Music, A Movies, B Shows`** |

  (The default order is by sort name: "A Movies" sorts as `movies`, the
  leading article being dropped, as Jellyfin does.)

## What S21 still has open

The other independent gaps stay on S21: the in-memory `GetResult` comparers for
null/date defaults, localized ratings, Studio numeric collation and user-data
sorts, BoxSet `DisplayOrder` pre-sort, and generated-key transliteration and
Unicode padding.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on core passed: core 2,485, livetv 299, server 237, extensions
96, wasm 62; real FFmpeg tests enabled, the WASM guest build disabled. Line
coverage over core's own sources: **95.15%**, above the 80% gate; the server
composition root is coverage-exempt. The instrumented build was cleaned
(`target` about 25 GiB, about 494 GiB free).
