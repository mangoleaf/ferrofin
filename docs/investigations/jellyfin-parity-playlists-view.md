# The Playlists folder and views: Ferrofin against Jellyfin

Investigation gl-1566, a phase of the backwards-migration milestone. Run on `release/v1.6.0` @ `978bea505` on
2026-11-10. The stored stamps quoted below are UTC, which had already reached 2026-11-11. Compared against Jellyfin 12.4 (`UPSTREAM_TAG` `v12.4`);
upstream paths are relative to the Jellyfin repository at that tag.

## Summary

The round trip of 2026-11-09 (Ferrofin v1.5.0) saw Ferrofin "touch" a Playlists row's `DateModified`.

- **Reproduced for the Playlists folder.** On a database Jellyfin wrote, Ferrofin's first boot sets the
  `PlaylistsFolder` row's `DateModified`, which Jellyfin had left NULL, to the boot's own time. The writer is the
  one-shot repair `repair_container_dates`, which v1.5.0 already shipped. Later boots leave the row alone.
- **Refuted for the per-user Playlists views.** At boot Ferrofin does not touch their `DateModified`: it stays
  NULL, as Jellyfin keeps it.
- **On a database Ferrofin writes itself,** both rows still differ from Jellyfin's:
  - Upstream saves each folder row through a refresh that stats its directory. A directory's stat carries no
    times, so `DateModified` is stored NULL, and the refresh stamps `DateLastRefreshed` and `DateLastSaved`.
  - Ferrofin gives neither row that refresh. Upstream skips it for the Playlists folder while its directory is
    empty (no playlist yet), so there the two already agree.

The verdict is to match upstream, the milestone's default (its decision D8: a difference from Jellyfin is fixed
unless the owner accepts it), filed as gl-1625.

## Observed

Both runs read the rows with:

```sql
SELECT "Type", "Name", "DateCreated", "DateModified", "DateLastRefreshed", "DateLastSaved"
FROM "BaseItems"
WHERE "Type" IN ('Emby.Server.Implementations.Playlists.PlaylistsFolder', 'MediaBrowser.Controller.Entities.UserView')
ORDER BY 1, 2;
```

### On a database Jellyfin wrote

1. Copy the synthetic `jellyfin-12.4-fresh` adoption fixture (the official image's own install, scan and seed, with
   four playlists) to a scratch directory.
2. Start `ferrofin-server --data-dir <copy> --bind 127.0.0.1 --port 18983` (built at `978bea505`).
3. Request `GET /UserViews?userId=<id>` with the fixture's `synthetic-admin` device token, the way the adoption
   gate's smoke test signs in. Then stop the server and run the statement.
4. Start the server again, make the same request, stop it, and run the statement again.

| Row | Column | Jellyfin wrote | After Ferrofin's first boot | After its second |
|---|---|---|---|---|
| `PlaylistsFolder` | `DateCreated` | `2026-11-04 07:53:51.5368306` | unchanged | unchanged |
| `PlaylistsFolder` | `DateModified` | NULL | `2026-11-11 02:48:56.7315386` | unchanged |
| `PlaylistsFolder` | `DateLastRefreshed` | `2026-11-04 07:54:06.7840892` | unchanged | unchanged |
| `PlaylistsFolder` | `DateLastSaved` | `2026-11-04 07:54:06.7841139` | unchanged | unchanged |
| both `Playlists` `UserView`s | every date | `DateModified` NULL; refreshed and saved | unchanged | unchanged |

The value written is the boot's own time; the directory's modification time was 2026-11-04 07:54:07. In this
fixture the folder's `DateLastRefreshed` (07:54:06.784) is just after the first playlists were created (07:54:06.65),
which is when upstream first refreshes the folder (below).

### On a database Ferrofin writes

1. Start the server on an empty data directory (`FERROFIN_ADMIN_PASSWORD=pw`) and sign in as `admin`.
2. Make a playlist (`POST /Playlists?Name=Mix&userId=<id>`). Then request `GET /UserViews?userId=<id>`, which lists
   `Playlists`, stop the server and run the statement.
3. Wait a few seconds, start it again, request the views again, stop it, and run the statement again.

Both boots leave the same rows:

```text
PlaylistsFolder | Playlists | 2026-11-11 02:44:17.5419932 | 2026-11-11 02:44:17.5419932 | NULL | 2026-11-11 02:44:17.5430596
UserView        | Playlists | 2026-11-11 02:44:52.8862400 | NULL                        | NULL | NULL
```

Compared with the Jellyfin fixture's rows (it had four seeded playlists, so this compares the state each server
reaches once playlists exist, not the same calls):

| Row | Column | Jellyfin 12.4 | Ferrofin |
|---|---|---|---|
| `PlaylistsFolder` | `DateCreated` | the directory's creation time | the directory's creation time |
| `PlaylistsFolder` | `DateModified` | NULL | the directory's modification time |
| `PlaylistsFolder` | `DateLastRefreshed` | set | NULL |
| `PlaylistsFolder` | `DateLastSaved` | set | set (at creation) |
| `Playlists` `UserView` | `DateCreated` | the view directory's creation time | the time of the request that made it |
| `Playlists` `UserView` | `DateModified` | NULL | NULL |
| `Playlists` `UserView` | `DateLastRefreshed` | set | NULL |
| `Playlists` `UserView` | `DateLastSaved` | set | NULL |

## The writers

### Jellyfin

- **When the Playlists folder is made.** `LibraryManager.CreateRootFolder` runs on the first access to `RootFolder`
  (`Emby.Server.Implementations/Library/LibraryManager.cs:218-232`):
  - it dates the folder from the directory it makes, `DateCreated = info.CreationTimeUtc` and
    `DateModified = info.LastWriteTimeUtc` (`:1116-1124`);
  - it saves the folder when it is not yet the root's child (`:1139-1144`);
  - it adds the folder to the root as a virtual child (`:1146`). `AggregateFolder.GetNonCachedChildren` returns the
    root's children with its virtual children (`MediaBrowser.Controller/Entities/AggregateFolder.cs:171-174`), so
    the library validation reaches the folder.
- **When the folder is refreshed.** `PerformLibraryValidation` validates the root recursively (`LibraryManager.cs:1490-1499`).
  - `Folder.ValidateChildrenInternal2` skips a top-parent child whose directory is empty or inaccessible, and keeps
    its row as it is (`MediaBrowser.Controller/Entities/Folder.cs:400-412`, `:484-493`).
  - `DirectoryService.IsAccessible` means at least one entry (`MediaBrowser.Controller/Providers/DirectoryService.cs:181-184`).
  - A `BasePluginFolder` is a top parent (`MediaBrowser.Controller/Entities/BaseItem.cs:770-777`), and only
    `collections` is exempted by name (`Folder.cs:402`). So the Playlists folder is first refreshed once its
    directory holds a playlist.
- **What that refresh stores.** `MetadataService.RefreshMetadata` (`MediaBrowser.Providers/Manager/MetadataService.cs`):
  - A first refresh (`DateLastRefreshed` is `MinValue`, `:95`) stamps `DateLastRefreshed` (`:223`).
  - `SaveInternal` then sets `item.DateModified = file.LastWriteTimeUtc` from the item's own path (`:244-256`).
  - `ManagedFileSystem.GetFileSystemMetadata` fills times only for a file
    (`Emby.Server.Implementations/IO/ManagedFileSystem.cs:257-260`), so a directory's are `MinValue`. The mapper
    stores `MinValue` as NULL (`Jellyfin.Server.Implementations/Item/BaseItemMapper.cs:308`).
  - The save stamps `DateLastSaved`. Every folder row of the fixture has `DateModified` NULL and every file-backed
    row has it set.
- **A per-user Playlists view.**
  - `UserViewManager` asks for it only when the user can see a playlist (`HasVisibleItem(Playlist)`,
    `Emby.Server.Implementations/Library/UserViewManager.cs:63-77`).
  - `LibraryManager.GetNamedView` (`LibraryManager.cs:3054-3117`) creates the view dated from its directory.
  - It queues a forced refresh (`ForceSave`) when the view is new, when 24 hours have passed since its last refresh
    (`_viewRefreshInterval`, `:101`), or when its display parent was saved since.
  - That refresh saves through the same `SaveInternal`: `DateModified` NULL, and `DateLastRefreshed` and
    `DateLastSaved` stamped.

### Ferrofin

- **The Playlists folder** is provisioned once at startup by `AggregateFolderStore::ensure`, through
  `repair_playlists_folder` (`crates/ferrofin-core/src/aggregate_folder.rs:245-373`). It is saved by
  `playlists_folder_row` (`:427-454`), dated from its directory as `CreateRootFolder` dates it, and never
  refreshed afterwards.
- **The directory rule already exists.** Ferrofin already ports the directory rule (a directory has no
  `DateModified`, owner decision D2) for rows the scan saves: `path_is_directory` and `date_modified_follows_save`
  (`crates/ferrofin-core/src/item_data.rs:571`, `:595`). The folder and the views never reach it.
- **The repair.** `repair_container_dates` (`crates/ferrofin-core/src/item_persistence_service.rs:1395-1472`, the
  one-shot marker `container_dates_v1`, added for gl-704):
  - It selects every `PlaylistsFolder` row with a NULL `DateCreated`, `DateModified` or `DateLastSaved`, and fills
    the NULLs.
  - On an adopted database that includes the `DateModified` Jellyfin's refresh left NULL.
  - It stats the stored path as written. Jellyfin's `%AppDataPath%/playlists` is not a path on disk, so the stat
    fails and the repair writes the boot's time.
  - Its doc says "a row Jellyfin wrote keeps its own". That holds only for the dates Jellyfin set.
- **A per-user Playlists view** is created by `ensure_user_playlists_view` (`crates/ferrofin-core/src/user_view_manager.rs:778`)
  on `GET /UserViews`. PR #914 (gl-1584) also creates it from unscoped item queries.
  - It is saved with `DateCreated` = now and no other date, and is never refreshed.
  - PR #914 records `GetNamedView`'s 24-hour refresh queue as not ported on either path, and notes it on gl-1566.

## Verdict

Fix to match upstream, filed as gl-1625:
1. Give the Playlists folder the first refresh upstream's library validation gives it once its directory holds a
   playlist: `DateModified` NULL by the directory rule, and `DateLastRefreshed` and `DateLastSaved` stamped. This
   sits beside the root folders' first refresh (PR #915, gl-1586).
2. Stop the repair from filling a NULL that a refreshed row holds, and stat the expanded path.
3. Port `GetNamedView`'s dates and its refresh for the per-user views.

After an export the rows heal themselves: the folder at Jellyfin's next library validation, and each view (it has
no parent, so validation never reaches it) at `GetNamedView`'s next refresh, on the user's next views listing. The
round-trip gate counts the difference before that.
