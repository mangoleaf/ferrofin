# New playlist and box set rows: NULL where Jellyfin stores a default

Investigation gl-1562, milestone "Backwards migration to Jellyfin". The milestone is the path back
from Ferrofin to Jellyfin: Jellyfin must open a database Ferrofin wrote and find each row as it
would have written it.

The `BaseItems` writer audit (gl-1558) saw that new Playlist and BoxSet rows from Ferrofin store
NULL in `Height`, `Width`, `Studios`, `Tags`, `ProductionLocations`, `ChannelId` and `MediaType`.
Jellyfin 12.4 stores `0`, `''`, the zero GUID and `Unknown` there. Jellyfin's first scan after a
return rewrote those rows, so the difference healed itself, but every such row differed on the
round trip.

**Verdict:** real, and not specific to these rows. It is the write path's general NULL-for-default
problem, which gl-1580 fixes in the open PR [#908](https://github.com/mangoleaf/ferrofin/pull/908).
With that change, the probed columns of the six rows below match what Jellyfin 12.4 stores: the
seven named above, plus `Genres` and `UnratedType`.

The milestone's decision D8 is to match upstream unless the owner accepts a divergence. This needs
no further fix and no divergence proposal. Two differences on the same rows lie outside this
investigation:
- the box set's name (below; gl-1598);
- the rows' dates (gl-1586).

## Reproduction

The same steps on three servers, each fresh, with one movie library holding one generated movie
(`ffmpeg -f lavfi -i testsrc=duration=2:size=320x240:rate=10 -c:v libx264 "Test Movie (2020).mkv"`):

1. `POST /Library/VirtualFolders?name=Movies&collectionType=movies&refreshLibrary=true` with
   `{"LibraryOptions":{"PathInfos":[{"Path":"<media>/movies"}]}}`, then wait until
   `GET /Items?Recursive=true&IncludeItemTypes=Movie` counts 1.
2. `POST /Playlists` with `{"Name":"Probe playlist","Ids":["<movie>"],"UserId":"<user>","MediaType":"Video"}`.
3. `POST /Collections?name=Probe%20collection&ids=<movie>`.
4. Wait 10 s for the box set's queued refresh, stop the server, and run:

```sql
SELECT Type, Name, quote(Height), quote(Width), quote(Genres), quote(Studios), quote(Tags),
       quote(ProductionLocations), quote(ChannelId), quote(MediaType), quote(UnratedType)
FROM BaseItems
WHERE Type IN ('MediaBrowser.Controller.Playlists.Playlist',
               'MediaBrowser.Controller.Entities.Movies.BoxSet',
               'Emby.Server.Implementations.Playlists.PlaylistsFolder',
               'MediaBrowser.Controller.Entities.CollectionFolder')
   OR (Type = 'MediaBrowser.Controller.Entities.Folder' AND Name = 'collections');
```

`quote()` prints NULL as `NULL` and an empty string as `''`. Type names are shortened below.

**Ferrofin, `release/v1.6.0` @ `978bea505` (before):**

| Type | Name | Height | Width | Genres | Studios | Tags | ProductionLocations | ChannelId | MediaType | UnratedType |
|---|---|---|---|---|---|---|---|---|---|---|
| CollectionFolder | Collections | NULL | NULL | NULL | NULL | NULL | NULL | NULL | NULL | NULL |
| CollectionFolder | Movies | NULL | NULL | NULL | NULL | NULL | NULL | NULL | NULL | NULL |
| Folder | collections | NULL | NULL | NULL | NULL | NULL | NULL | NULL | `'Unknown'` | `'Other'` |
| BoxSet | P.R.O.B.E. Collection | NULL | NULL | NULL | NULL | NULL | NULL | NULL | NULL | `'Movie'` |
| Playlist | Probe playlist | NULL | NULL | NULL | NULL | NULL | NULL | NULL | `'Video'` | `'Other'` |
| PlaylistsFolder | Playlists | NULL | NULL | NULL | NULL | NULL | NULL | NULL | NULL | NULL |

**Ferrofin with gl-1580 (#908 @ `6628f9c2`, after):** the same six rows.
- Every row stores `0`, `0`, `''`, `''`, `''`, `''` and `'00000000-0000-0000-0000-000000000000'`.
- `MediaType` is `'Unknown'`, except the playlist's `'Video'`.
- `UnratedType` is `'Other'`, except the box set's `'Movie'`.
- The box set is still named "P.R.O.B.E. Collection" (see below).

**Jellyfin 12.4 (`jellyfin/jellyfin:12.4`, after the setup wizard):**

| Type | Name | Height | Width | Genres | Studios | Tags | ProductionLocations | ChannelId | MediaType | UnratedType |
|---|---|---|---|---|---|---|---|---|---|---|
| CollectionFolder | Collections | 0 | 0 | `''` | `''` | `''` | `''` | `'00000000-…'` | `'Unknown'` | `'Other'` |
| CollectionFolder | Movies | 0 | 0 | `''` | `''` | `''` | `''` | `'00000000-…'` | `'Unknown'` | `'Other'` |
| Folder | collections | 0 | 0 | `''` | `''` | `''` | `''` | `'00000000-…'` | `'Unknown'` | `'Other'` |
| BoxSet | Probe collection | 0 | 0 | `''` | `''` | `''` | `''` | `'00000000-…'` | `'Unknown'` | `'Movie'` |
| Playlist | Probe playlist | 0 | 0 | `''` | `''` | `''` | `''` | `'00000000-…'` | `'Video'` | `'Other'` |
| PlaylistsFolder | Playlists | 0 | 0 | `''` | `''` | `''` | `''` | `'00000000-…'` | `'Unknown'` | `'Other'` |

The audit's observation is reproduced, and it reaches further:
- the four container rows have the same gap as the box set and the playlist: the playlists folder,
  the Movies library, and the Collections library's `CollectionFolder` and location `Folder`;
- so does `Genres`, which the issue did not name.

Only `MediaType` and `UnratedType` were ever right before the fix, and only on some rows:
- the playlist's `Video` and `Other`;
- the box set's `UnratedType` `Movie`;
- the location folder's `Unknown` and `Other`, written by a later save of the row (its INSERT has
  neither, below).

The box set's `MediaType` was not right: it was NULL where Jellyfin stores `Unknown`.

The box set's **name** differs for a reason outside this investigation, on the base build and on
#908's alike:
- On Ferrofin, the library scan that the first collection starts refreshes the new box set before its
  queued `FullRefresh`. On a first refresh in Default mode, upstream's merge rule replaces stored
  values, so TMDb's name for collection 446149 replaces the one the user typed.
- Jellyfin's queued `FullRefresh` kept it.
- A second collection, created once the library existed, kept its name on Ferrofin too.

The evidence is recorded on gl-1598, which owns that scan.

## The writers

**Jellyfin 12.4** writes every column on every save, from the item's properties:
- **Create:** `CreatePlaylist` builds the `Playlist`, sets its media type and calls
  `parentFolder.AddChild(playlist)` (`Emby.Server.Implementations/Playlists/PlaylistManager.cs:138-150`).
  `CreateCollectionAsync` does the same for the `BoxSet` (`Emby.Server.Implementations/Collections/CollectionManager.cs:181-191`).
- **Save:** `Folder.AddChild` calls `LibraryManager.CreateItem` (`MediaBrowser.Controller/Entities/Folder.cs:230-249`).
  That calls `CreateItems` and `SaveItems` (`Emby.Server.Implementations/Library/LibraryManager.cs:2440-2446,2491`), which
  maps each item with `BaseItemMapper.Map` (`Jellyfin.Server.Implementations/Item/ItemPersistenceService.cs:173-175,341`).
- **The mapper**, `Jellyfin.Server.Implementations/Item/BaseItemMapper.cs`, with property defaults from
  `MediaBrowser.Controller/Entities/BaseItem.cs`:
  - `ChannelId`, `Width` and `Height` are copied as they are (mapper :309, :313-314): a `Guid` and two
    `int`s, so `00000000-…` and `0` when unset (`BaseItem.cs:187,426,428`).
  - `Genres`, `ProductionLocations`, `Studios` and `Tags` are joined with `|` (mapper :306, :332-334). The
    constructor sets each list to an empty array (`BaseItem.cs:117-123`), so `''`.
  - `MediaType` is the enum's name (mapper :405). It is `Unknown` by default (`BaseItem.cs:474`), and a
    playlist's own `PlaylistMediaType` (`MediaBrowser.Controller/Playlists/Playlist.cs:84-87`).
  - `UnratedType` is `GetBlockUnratedType()` (mapper :411): `Movie` for a box set
    (`MediaBrowser.Controller/Entities/Movies/BoxSet.cs:81-84`). For the other rows probed it is
    `BaseItem`'s default `Other` (`BaseItem.cs:2033-2041`, which gives `ChannelContent` for a channel item).

**Ferrofin on `release/v1.6.0` @ `978bea505`:**
- **Create:** both items are created by `insert_named_item_in`
  (`crates/ferrofin-core/src/item_persistence_service.rs:423`), the box set from `create_collection`
  (`crates/ferrofin-core/src/collection_manager.rs:736`) and the playlist from `create_playlist`
  (`collection_manager.rs:1484`). The INSERT lists 18 columns and none of the eight list, size, channel
  and media columns above, so SQLite leaves them NULL.
- **Later saves:** they go through the upsert. Its `columns_of`
  (`crates/ferrofin-core/src/item_persistence_service.rs:1943`) binds an absent value as NULL.
- **Container rows:**
  - the playlists folder is saved by `repair_playlists_folder` from `playlists_folder_row`
    (`crates/ferrofin-core/src/aggregate_folder.rs:311-316,427`) through the upsert. `insert_root_folder`
    (`aggregate_folder.rs:214`) creates only the root `AggregateFolder` row;
  - a library's `CollectionFolder` row, the Collections library's included, is saved by
    `ensure_collection_folder` (`crates/ferrofin-core/src/virtual_folder_manager.rs:690`) through the
    upsert;
  - the Collections library's location `Folder` row comes from `ensure_folder`
    (`crates/ferrofin-core/src/collections_library.rs:253-276`) → `insert_location_folder` →
    `ensure_location` (`crates/ferrofin-core/src/library_structure_repository.rs:142`), whose INSERT
    omits the same columns.

The issue names `ensure_container` as a starting point. It no longer exists on this base: the
writers above replaced it.

gl-1580 changes all of these:
- `columns_of` derives upstream's default whenever the item has none, which covers the upsert writers;
- every raw insert, these included, lists the same column fragment and the kind's `UnratedType`;
- a stored zero GUID or `''` reads back as no value, so the DTOs do not change.

## Does the difference matter?

- **To a Ferrofin client:** mostly not. Every DTO treats NULL and the default alike: it maps a size
  only when it is above 0, and an empty list as `[]`.
- **To two query filters:** yes, and gl-1580 brings both to Jellyfin's behaviour for these rows:
  - `MediaTypes=Unknown` (`bi."MediaType" IN (…)`, `crates/ferrofin-core/src/translate_query.rs:1230`)
    never matches a NULL row, but does match Jellyfin's stored `Unknown`;
  - `MaxWidth`/`MaxHeight` (`bi."Width" <= ?`, `translate_query.rs:1362`) leave out a NULL row, but include a `0` one.
- **To a return to Jellyfin:** it does. Each row differs from what Jellyfin writes, and Jellyfin's first
  scan after the return rewrites it. The round-trip gate (gl-1568) diffs every row of Ferrofin's
  database with what Jellyfin leaves after its scan. A rewritten row fails that diff unless the owner
  accepts it into the allowlist (gl-1573).
- **After gl-1580:** these columns match, so they leave nothing to accept or file.
