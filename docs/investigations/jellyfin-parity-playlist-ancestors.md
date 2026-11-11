# A new playlist's AncestorIds: Ferrofin against Jellyfin

Investigation gl-1561, a phase of the backwards-migration milestone. Run on `release/v1.6.0` @ `978bea505` on
2026-11-10. Compared against Jellyfin 12.4 (`UPSTREAM_TAG` `v12.4`); upstream paths are relative to the Jellyfin
repository at that tag.

## Summary

The export-and-return test of 2026-11-09 (Ferrofin v1.5.0) saw Jellyfin's first scan after the return add two
`AncestorIds` rows to a playlist Ferrofin had created: the Playlists folder and the root. The Ferrofin half
reproduces here:

- **A playlist made with `POST /Playlists` gets no `AncestorIds` rows in Ferrofin,** after the create or after a
  library scan.
- **The Playlists folder has none either.**

Jellyfin's code writes a playlist's two rows when it is created, and the folder's one, the root, when the folder
is. A Jellyfin 12.4 database holds exactly those rows.

The missing rows change one Ferrofin response, through a handler bug, and no other.
- **The one that changes:** an API key's browse of the Playlists folder, with no `userId`, returns no playlist made
  through the API. With no user, upstream lists the folder's direct children and reads no `AncestorIds`. Ferrofin's
  handler makes the query recursive, and so needs the rows. That handler difference is filed separately as gl-1627.
- **Every other browse reads no playlist's own rows:**
  - a user's browse of the Playlists folder is re-rooted to the user root;
  - a recursive browse of the root drops the parent;
  - a root browse typed exactly `Playlist` is redirected, on both servers, to the playlists' linked children.

The rows matter to Jellyfin after an export. Its recursive listing under the root scopes by `AncestorIds`, unless the
listing is typed exactly `Playlist` or exactly `BoxSet`.

The verdict is to match upstream, the milestone's default (its decision D8: a difference from Jellyfin is fixed
unless the owner accepts it), filed as gl-1626.

## Observed

This statement lists every playlist, Playlists folder and box set with its ancestors, and counts the items that
have none. It runs unchanged on either database:

```sql
SELECT substr(i."Type", length(rtrim(i."Type", replace(i."Type", '.', ''))) + 1) AS type, i."Name",
       count(a."ParentItemId") AS ancestors,
       group_concat(substr(p."Type", length(rtrim(p."Type", replace(p."Type", '.', ''))) + 1), ', ') AS parents
FROM "BaseItems" i
LEFT JOIN "AncestorIds" a ON a."ItemId" = i."Id"
LEFT JOIN "BaseItems" p ON p."Id" = a."ParentItemId"
WHERE i."Type" IN ('MediaBrowser.Controller.Playlists.Playlist',
                   'Emby.Server.Implementations.Playlists.PlaylistsFolder',
                   'MediaBrowser.Controller.Entities.Movies.BoxSet')
GROUP BY i."Id" ORDER BY 1, 2;
```

### Ferrofin

1. Start `FERROFIN_ADMIN_PASSWORD=pw ferrofin-server --data-dir <dir> --bind 127.0.0.1 --port 18984` on an empty
   data directory (built at `978bea505`).
2. Sign in with `POST /Users/AuthenticateByName` and the body `{"Username": "admin", "Pw": "pw"}`. Send the token on
   every later request:
   `Authorization: MediaBrowser Client="probe", Device="probe", DeviceId="probe", Version="1", Token="<token>"`.
3. Make a playlist, `POST /Playlists?Name=Mix&userId=<id>`, and for comparison a collection,
   `POST /Collections?name=Saga`.
4. Run a library scan (`POST /Library/Refresh`) and wait for the `RefreshLibrary` task to finish.
5. Stop the server and run the statement on `<dir>/ferrofin.db`. It gives the same rows before the scan.

```text
BoxSet | Alagoan Crime Saga | 3 | Folder, CollectionFolder, AggregateFolder
Playlist | Mix | 0 | NULL
PlaylistsFolder | Playlists | 0 | NULL
```

The box set's name is the one a metadata refresh gave it (gl-1622); its ancestors are the point here.

6. Start the server again. Make an API key (`POST /Auth/Keys?app=probe`, then read it from `GET /Auth/Keys`); it is
   sent as `Authorization: MediaBrowser Token="<key>"`. The ids are the `Id`s of the `PlaylistsFolder` and
   `AggregateFolder` rows in `BaseItems`, written without hyphens. Then request `GET /Items?Recursive=true` with
   these query parameters:

| Authorization | Query parameters | Playlists listed |
|---|---|---|
| the user's token | `userId=<id>&ParentId=<Playlists folder>` | `Mix` |
| the API key | `ParentId=<Playlists folder>` | none |
| the API key | `ParentId=<root>` | `Mix`, and the root and user root rows themselves (outside the root's descendants: the parent was dropped) |
| the API key | `ParentId=<root>&IncludeItemTypes=Playlist,BoxSet` | `Mix` |
| the API key | `ParentId=<root>&IncludeItemTypes=Playlist` | none |
| the API key | `IncludeItemTypes=Playlist` | `Mix` |
| the user's token | `userId=<id>&ParentId=<root>` | `Mix`, the box set, the `collections` folder and the Playlists folder |

Ferrofin's handler, `list_playlists_folder_as_playlists` (`crates/ferrofin-api/src/handlers/items.rs:889-901`), sets
the listing recursive in every case:
- With a user, it re-roots the listing to the user root, which reads no `AncestorIds`.
- Without one, the Playlists folder stays the parent of a recursive query. That query requires each playlist to have
  an `AncestorIds` row naming the folder (`crates/ferrofin-core/src/translate_query.rs:948-957`).

The handler's own doc says that with no user it lists the folder's own children only, as upstream does; the code
does not.

What Jellyfin answers here comes from its code, not a probe:
- **The Playlists folder, no user.** `PlaylistsFolder.GetItemsInternal` sets `Recursive = false`
  (`Emby.Server.Implementations/Playlists/PlaylistsFolder.cs:37-43`). The folder's direct children, by `ParentId`,
  are listed with or without `AncestorIds` rows.
- **The Playlists folder, with a user.** The recursive query is scoped by `TopParentIds`: a `BasePluginFolder` is an
  `ICollectionFolder`, so `SetTopParentIdsOrAncestors` takes its first branch (`LibraryManager.cs:2043-2058`). The
  fixture's playlists have the Playlists folder as their `TopParentId`.
- **The root, typed exactly `Playlist`.** `ItemsController` turns the request into a scope over the playlists'
  linked children (`Jellyfin.Api/Controllers/ItemsController.cs:291-308`). That reads the children's ancestors, not the
  playlist's own rows. `Mix` is empty, so neither server lists it there; Ferrofin's `redirect_container_browse` is the
  port of this redirect.
- **The root, otherwise** (untyped, or typed with anything but exactly `Playlist` or exactly `BoxSet`, such as
  `Playlist,BoxSet`). `AggregateFolder` is neither an `ICollectionFolder` nor a `UserView`, so the recursive listing
  goes through `SetAncestorIds` (`:2070-2073`). Jellyfin lists a playlist there only if its rows exist. Ferrofin's
  recursive browse of the root drops the parent and does not read them, so it lists `Mix`.

### Jellyfin 12.4

The same statement on a copy of the synthetic `jellyfin-12.4-fresh` adoption fixture's `data/jellyfin.db` (the
official image's own install, scan and seed):

```text
BoxSet | Synthetic collection | 3 | Folder, CollectionFolder, AggregateFolder
BoxSet | Synthetic linked collection | 3 | Folder, CollectionFolder, AggregateFolder
Playlist | Synthetic linked playlist | 2 | PlaylistsFolder, AggregateFolder
Playlist | Synthetic mixed order | 2 | PlaylistsFolder, AggregateFolder
Playlist | Synthetic path-less playlist | 2 | PlaylistsFolder, AggregateFolder
Playlist | Synthetic private playlist | 2 | PlaylistsFolder, AggregateFolder
PlaylistsFolder | Playlists | 1 | AggregateFolder
```

The box sets match Ferrofin's. This database shows the rows once written, not when.

## The writers

### Jellyfin

- **`PlaylistManager.CreatePlaylist`** adds the playlist to the Playlists folder, `parentFolder.AddChild(playlist)`
  (`Emby.Server.Implementations/Playlists/PlaylistManager.cs:150`), then refreshes it with `ForceSave` (`:152-153`).
- **`Folder.AddChild` creates the item.** It calls `LibraryManager.CreateItem`
  (`MediaBrowser.Controller/Entities/Folder.cs:230-250`), which saves it through `_persistenceService.SaveItems`
  (`Emby.Server.Implementations/Library/LibraryManager.cs:2491`).
- **Every save writes the item's ancestors.** `ItemPersistenceService.UpdateOrInsertItems` collects
  `item.GetAncestorIds()` for an item that supports ancestors
  (`Jellyfin.Server.Implementations/Item/ItemPersistenceService.cs:321-323`). It then writes the rows, keeping only
  ancestors that exist and removing stale ones (`:424-477`).
- **What `GetAncestorIds` returns.** It is the item's parents (`GetParents`, walking `ParentId`) followed by its
  collection folders (`MediaBrowser.Controller/Entities/BaseItem.cs:2943-2946`, `:1116-1126`;
  `LibraryManager.GetCollectionFolders`, `LibraryManager.cs:2846-2887`).
  - A playlist's parents are the Playlists folder and the aggregate root.
  - Its collection folders are none, so its rows are those two.
- **The Playlists folder** is saved by `CreateRootFolder` with the root as its parent (`LibraryManager.cs:1139-1144`),
  so its one row is the root.

### Ferrofin

- **`create_playlist`** (`crates/ferrofin-core/src/collection_manager.rs:1443-1532`) writes, in one transaction:
  - the row (`insert_named_item_in`) and its path;
  - the owner and share rows, the entries, the media type and the `Data` sync.

  It writes no ancestors.
- **`create_collection`** beside it does, with `add_ancestors_in` (`:749`;
  `crates/ferrofin-core/src/library_structure_repository.rs:271-288`). Its ancestors come from the box set's folder
  (`collection_manager.rs:710-721`, `CollectionsLibrary::ancestors`,
  `crates/ferrofin-core/src/collections_library.rs:287`): the root, the Collections library and the `collections`
  folder.
- **The Playlists folder** is built by `playlists_folder_row` (`crates/ferrofin-core/src/aggregate_folder.rs:427-454`)
  and saved without ancestors (`:311-316`). The paths that re-parent an existing folder onto the root
  (`reparent_virtual_child`, `crates/ferrofin-core/src/item_persistence_service.rs:821-839`) write none either.
- **The scan.** Its ancestor writer, `set_ancestors` (`item_persistence_service.rs:3014`), writes them for the items
  it saves. It never saves an API-made playlist or the Playlists folder, so the scan does not fill them in.

## Verdict

Fix to match upstream, filed as gl-1626:
1. Write the playlist's ancestors in `create_playlist`'s transaction, as `create_collection` does.
2. Write the Playlists folder's where it is saved and re-parented.
3. Repair the rows earlier builds left, idempotently. Rows from an earlier build are what the 2026-11-09 test met.

After an export, Jellyfin's next save of a playlist writes its rows. Until then Jellyfin's recursive listing under the
root misses the playlist, unless the listing is typed exactly `Playlist` or exactly `BoxSet`. The row-level comparison
the milestone plans (gl-1573) counts the missing rows.

The handler's difference is a separate fix, gl-1627: make the Playlists-folder listing non-recursive without a user,
as `PlaylistsFolder.GetItemsInternal` does and as the handler's doc already says. Writing the rows alone would make
that listing return the playlist, but would hide the difference rather than remove it. The handler difference is
outside the row comparison.
