# S25: a box set's `DisplayOrder`

`BoxSet.GetChildren` sorts the set's direct members by its `DisplayOrder`
(`BoxSet.cs:116-132`), and its constructor makes the default `PremiereDate`.
Ferrofin listed a set's members by name whatever the set said, returned no
`DisplayOrder` for a set that had not stored one, and — found while verifying —
**ignored the `DisplayOrder` the metadata editor posts** for a box set or a
series (the update route never wrote it, so the editor's "Display order" select
did nothing; a series' change also never queued the refresh the editor expects).

Oracle: Jellyfin 12.2 (portable build) on the same media: a set created from
Zeta (2001), Alpha (2003), Mid (1999), Beta (2002), in that order.

## Behavior

- A direct listing of a box set with no `SortBy` is ordered by the set's
  `DisplayOrder`: `Enum.TryParse<ItemSortBy>` of the stored text (a name,
  case-sensitively, or a number) ascending; a set that stored none, and any text
  that is not an `ItemSortBy`, order by `PremiereDate`; `Default` lists the
  members in the order they were added (the container's linked-children order).
  An order the request names (`SortBy=…`) wins, as upstream. A recursive listing is
  the repository query and ignores the set's order — measured on the oracle, not
  assumed.
- The DTO of a box set carries `DisplayOrder`: the stored text, `PremiereDate`
  for a set that stored none (the constructor's default).
- `POST /Items/{id}` writes `DisplayOrder` for a series and a box set (as
  `IHasDisplayOrder.DisplayOrder = request.DisplayOrder`, a null clearing it) and
  for no other kind, and a **series** whose order changed (case-insensitively, null
  as empty) queues the `FullRefresh`/`ReplaceAllMetadata` refresh upstream queues so
  its episodes are renumbered under the new order (`ItemUpdateController.cs:
  83-86,120-132`); a box set's change queues none.
- The item-refresh route and the update route share one `queue_item_refresh`.

## Verification

- Core: one set listed under no stored order (premiere order; request `SortName`
  wins), `SortName`, a numeric `10`, unparseable texts (`premieredate`, empty,
  `Nonsense` → premiere order) and `Default` (added order), and a recursive listing
  (name order throughout).
- API: the stored text for a box set and a series (and its clearing), no
  `DisplayOrder` on a movie, and the series refresh only when the order really
  changed — case-only, null-vs-null and box-set changes queue none.
- Against Jellyfin 12.2, the same probe (`GET /Users/{u}/Items?parentId=<set>`):

  | | Jellyfin 12.2 | Ferrofin before | Ferrofin after |
  |---|---|---|---|
  | no `SortBy` | `Mid, Zeta, Beta, Alpha` | `Alpha, Beta, Mid, Zeta` | `Mid, Zeta, Beta, Alpha` |
  | `SortBy=SortName` / `…Descending` | by name / reversed | same | same |
  | DTO `DisplayOrder` | `PremiereDate` | absent | `PremiereDate` |
  | after posting `SortName` | name order | name order (not stored) | name order |
  | after posting `Default` | `Zeta, Alpha, Mid, Beta` | name order | `Zeta, Alpha, Mid, Beta` |
  | recursive, no `SortBy` | name order | name order | name order |

- Music, same probe (one artist, two albums): the listing of the artist, of each
  album and their recursive forms are identical to Jellyfin's already, so the
  music half of this finding has nothing to port.

## Limitations

- A set's listing sorts in SQL (`PremiereDate`, `SortName`, …); Jellyfin sorts
  in memory with the comparers the S21 finding tracks, so ties and null dates
  inherit whatever S21 still differs on.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on the changed crates passed: core 2,487, api 937, mediaencoding
954, providers 926 (4 skipped), livetv 299, server 237, hls 117, extensions 96,
wasm 62; real FFmpeg tests enabled, the WASM guest build disabled. Clippy first
flagged an over-long folder-display function and a helper placed after the test
module; the box-set ordering condition moved into its helper and the helper
before the module, then fmt, Clippy, the boundary test and core's and api's tests
and doctests were rerun clean. Separate line coverage over each changed crate's
own sources, measured before that move: core **95.16%**, api **88.02%**, both
above the 80% gate; the traits crate (one internal query field) and the server
composition root are coverage-exempt. Instrumented builds were cleaned
(`target` about 25 GiB).
