# S03: plain subfolders of a movie library are `Folder`s

A movie library's plain subfolders — a genre or decade directory that is no
title's own folder (`Action/`, `Action/90s/`) — were flattened: their titles
hung straight off the library and the directory was no item at all, so a
"Folders" view (and `GET /Items?ParentId=<library>`) could not browse into it.
Jellyfin's `FolderResolver` claims any directory no other resolver does
(`FolderResolver.cs:24-27`, last priority), and `Folder.GetNonCachedChildren`
resolves what is inside it with the folder as parent
(`ResolvePaths(children, …, this, …)`). The old code carried the gap as an
explicit TODO naming this exact path.

Oracle: Jellyfin 12.2 (portable build, run against the same trees as Ferrofin;
the pinned source is `4910aafa1a`) — see the comparison below.

## Behavior

- `collect_movie_plan` (movies, music videos and home videos) plans each
  directory that is not a title's own folder, not a disc rip, and not inside a
  title's own folder (whose subfolders hold its extras) as a `Folder` row:
  `ParentId` is the folder above (the location for a top-level one), `TopParentId`
  stays the library, and what is in it is planned with that folder as parent and
  an ancestor closure that runs through it. Titles keep every other rule (own
  folder → the movie, named from the folder; disc rips; extras; samples).
- An empty directory, or one whose only video is an ignored sample, is a folder
  too (as Jellyfin lists it). A title whose file is gone leaves its directory as a
  plain folder. A path-scoped scan keeps the folders above the scoped path as
  context rows, as it keeps every row above.
- **Existing databases upgrade in place**: the next scan creates the folder rows
  and re-parents the titles under them; ids, user data and metadata are kept (the
  row's identity is its path).

## Verification

- Core tests: a library with `Action/Heat`, `Action/90s/Ronin` (+ `extras/`),
  `Top` in its own folder, a loose movie and an empty directory — folder rows
  with the right parents, tops and ancestor rows, no row for a title's own folder
  or its `extras/`, listings of the library and of each folder, an unchanged
  rescan (nothing created/updated/removed) and the removal of one title leaving its
  folder and its siblings. The existing scanner suites were updated where they had
  pinned the flattened shape (the adopted-library fixture, the nested-location
  test, the extras browse counts, an emptied title folder and a refresh of a gone
  item, each now with a folder row Jellyfin also has); `plan_paths_matches_the_filtered_full_plan`
  (scoped plan = filtered full plan for every fixture path) still holds.
- Production binary, `Action/{Heat,Ronin}`, `Top`, a loose file:

  | | `GET /Items?ParentId=<library>` | `Action` |
  |---|---|---|
  | before | `Heat, Loose Movie, Ronin, Top` (all flat) | — |
  | after | **`Action` (Folder), `Loose Movie`, `Top (2001)`** | **`Heat`, `Ronin`** |

  Upgrade: a library scanned by the previous build and rescanned by this one keeps
  the ids and a favorite and re-parents `Heat`/`Ronin` under `Action`.
- **Against Jellyfin 12.2**, the same media tree with `Outer` = the whole tree
  and `Inner` = `…/Kids` (nested), three scans each, then removing `Inner`:

  | | Jellyfin 12.2 | Ferrofin |
  |---|---|---|
  | Outer, direct children | `Action, Empty, Kids` (Folders), `Loose Movie`, `Top (2001)` | identical |
  | Outer, recursive | `90s, Action, Coco, Empty, Heat, Kids, Loose Movie, Ronin, Sub, Top, Up, movies` | identical |
  | Inner, direct children | `Sub` (Folder), `Up (2009)` | identical |
  | Inner, recursive movies | none | none |
  | Folder rows (name → parent) | `Action`,`Empty`,`Kids` → the location; `90s` → `Action`; `Sub` → `Kids` | identical shape |
  | After removing `Inner` | `Outer` unchanged | unchanged |

  (The location folder `movies` carries the same id prefix in both.)

## Limitations

- Only the movie-style planner (movies, music videos, home videos). The TV,
  music, book and photo planners are untouched here: their intermediate
  directories are series, seasons, artists and albums, or are not probed against
  the oracle in this change.
- "Inherited policies" for an inner library (parental rating, tag filters on a
  nested library) is not separately probed.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on core passed: core 2,486, livetv 299, server 237,
extensions 96, wasm 62; real FFmpeg tests enabled, the WASM guest build
disabled. The first run's Clippy flagged the new test as over-long (one tree,
every property of it, so it carries an allow); fmt, Clippy, the boundary test and
core's tests and doctests were rerun clean after. Line coverage over core's own
sources: **95.15%**, above the 80% gate; the server composition root is
coverage-exempt. The instrumented build was cleaned (`target` about 25 GiB).
