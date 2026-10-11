# S21b: recursive browse ordering — `Studio`, `Artist`, `AlbumArtist`, `IsPlayed`

A second slice of S21. With a Jellyfin 12.2 oracle available, every `SortBy` key
was compared on the same library (six movies with and without premiere date,
rating, certificate, runtime, studio, critic rating; two played, one favorited),
ascending and descending, for a **recursive** listing (the repository query,
`OrderMapper`) and a **direct** listing of the library (the in-memory
`LibraryManager.Sort`).

Recursive listings: every key matched except four, now fixed. Direct listings:
see "What S21 still has open".

## Behavior

- `Studio`, `Artist` and `AlbumArtist` order by the item's first `ItemValues`
  entry of that type by its clean text (`OrderMapper.cs:61-66`), an item with none
  first — Ferrofin had fallen back to `SortName` for all three (the code carried
  it as an open work item). The comparison is ordinal on the clean text, so
  `alpha studio 10` precedes `alpha studio 2`, as Jellyfin's query does.
- `IsPlayed`/`IsUnplayed` read a missing `UserData` row as not played
  (`COALESCE(…, 0)`, as `IsFavoriteOrLiked` already did): an item the user has no
  row for ties with one whose row says unplayed and lists by name, instead of
  sorting ahead of it as a SQL `NULL` does.

## Verification

- Core: five movies with studios (`Zed`; `Zed`+`Alpha Studio 10`; none;
  `Alpha Studio 2`; `Beta`) and artists, ordered by `Studio` both ways and by
  `Artist`; an unplayed row, a missing row and two played rows ordered by
  `IsPlayed`/`IsUnplayed` both ways.
- Against Jellyfin 12.2, the same 18 keys × 2 directions, **recursive**: all 36
  listings now identical (before: `Studio` ×2 and `IsPlayed`/`IsUnplayed` ×4
  differed).

## What S21 still has open

- *Direct* listings of a library (the in-memory comparers): `Studio` compares the
  first studio with numeric ordering (`alpha studio 2` before `…10`, via
  `CompareOptions.NumericOrdering`), `OfficialRating` descending keeps equal
  scores in their original order (the oracle lists the unrated `Charlie` before the
  `G`-rated `Delta`; Ferrofin reverses that tie), and generated-key
  transliteration/Unicode padding are not yet ported.
- The oracle's direct listings **ignore** `PlayCount`, `IsPlayed`, `IsUnplayed`,
  `IsFavoriteOrLiked` and `DatePlayed` (the order stays by name in both
  directions) although the comparers exist in the pinned source — an upstream
  defect as far as can be told. Ferrofin sorts by them. Whether to reproduce a
  no-op is the owner's decision, not made here.
- Per-user `OrderedViews` is S21a.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on core passed: core 2,488, api 937, livetv 299, server 237,
extensions 96, wasm 62; real FFmpeg tests enabled, the WASM guest build
disabled. Clippy first flagged the extended play-state test as over-long (one
user and library, every order — it carries an allow); fmt, Clippy, the boundary
test and core's tests and doctests were rerun clean after. Line coverage over
core's own sources: **95.16%**, above the 80% gate; the server composition root
is coverage-exempt. The instrumented build was cleaned (`target` about 25 GiB).
