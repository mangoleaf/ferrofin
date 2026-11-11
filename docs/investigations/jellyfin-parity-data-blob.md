# `BaseItems.Data`: what Ferrofin writes against Jellyfin's serialised item

Investigation gl-1563. The question is whether a database Ferrofin wrote reads back correctly in Jellyfin, and whether Jellyfin rewrites its rows when it does. Upstream citations are to Jellyfin `v12.4`. Ferrofin citations are to `release/v1.6.0` @ `978bea505`.

Jellyfin stores each item's properties as JSON in `BaseItems.Data`. `Jellyfin.Server.Implementations/Item/BaseItemMapper.cs:266-268` serialises the whole item on every save, for every type without `[RequiresSourceSerialisation]` (`:485-488`). On read, `:485-529` deserialises that JSON into the item, and `:78-249` then overlays the mapped columns. So each property has one of three fates when Jellyfin reads a Ferrofin-written row:
- **column-backed:** overlaid from a column, so the copy in `Data` is never read;
- **`Data`-only:** read from `Data`; if Ferrofin omits it, Jellyfin takes the property's default;
- **computed or get-only:** ignored on read.

Thirteen kinds carry `[RequiresSourceSerialisation]`, so Jellyfin never writes or reads their `Data`: PlaylistsFolder, MusicAlbum, MusicArtist, Genre, MusicGenre, Studio, Person, Year, Season, Book, AudioBook, PhotoAlbum and LiveTvProgram (for example `Emby.Server.Implementations/Playlists/PlaylistsFolder.cs:15`, `MediaBrowser.Controller/Entities/TV/Season.cs:24`, `MediaBrowser.Controller/Entities/Book.cs:13`).

**Verdict, for the kinds probed:**
- **One value a reader sees is lost.** A video's `HasSubtitles` is `Data`-only, and Ferrofin never stores it. A returned video with subtitles reads it as false until its file changes or is fully refreshed. Fix: gl-1632.
- **One value does not heal.** A root folder Ferrofin created has no `Data`, so it reads back with `IsRoot` false. Nothing in 12.4 sets it true on a stored root. Its effect is narrow (see below). Fix: gl-1629.
- **Everything else is churn, not loss.** Every other difference is a column Jellyfin overlays, a default that is the right value, or a value Jellyfin ignores, rebuilds or recomputes. Jellyfin still rewrites each such row's `Data` on its first save of it. The fix, per the owner's rule to match upstream unless the owner accepts a divergence, is gl-1630. That issue also names allowlisting the difference as the owner's alternative.
- **Not probed:** Series, Episode, Photo, Trailer, MusicVideo and extras. gl-1630 owns them.

## Reproduction

The same steps on two fresh servers: Ferrofin built from `release/v1.6.0` @ `978bea505` (database `{data}/ferrofin.db`), and stock `jellyfin/jellyfin:12.4` after its setup wizard (`/config/data/jellyfin.db`). The media:
- a two-second movie with one embedded SRT subtitle, `ffmpeg -f lavfi -i testsrc=duration=2:size=320x240:rate=10 -i sub.srt -map 0 -map 1 -c:v libx264 -c:s srt "movies/Test Movie (2020)/Test Movie (2020).mkv"`;
- a two-second track, `ffmpeg -f lavfi -i sine=frequency=440:duration=2 -metadata title="Test Track" -metadata artist="Test Artist" -metadata album="Test Album" -metadata track=1 "music/Test Artist/Test Album/01 Test Track.flac"`.

1. `POST /Library/VirtualFolders?name=Movies&collectionType=movies&refreshLibrary=true` and `?name=Music&collectionType=music&refreshLibrary=true`, each with `{"LibraryOptions":{"PathInfos":[{"Path":"<media>/<movies|music>"}]}}`. Wait until both items are listed.
2. `POST /Playlists` with `{"Name":"Probe playlist","Ids":["<movie>"],"UserId":"<user>","MediaType":"Video"}`.
3. `POST /Collections?name=Probe%20collection&ids=<movie>`.
4. `GET /UserViews?userId=<user>`.
5. Wait 20 s and stop the server.

On either database, the keys each kind's `Data` holds, and how many of its rows hold none:

```sql
SELECT BaseItems.Type, json_group_array(DISTINCT j.key)
FROM BaseItems, json_each(BaseItems.Data) AS j
GROUP BY BaseItems.Type;

SELECT Type, SUM(Data IS NULL), COUNT(*) FROM BaseItems GROUP BY Type;
```

The rows were also compared value by value. Values that only reflect each server's own paths and ids are left out below: user ids, `PhysicalLocationsList`, `PhysicalFolderIds`. A library location's `Folder` is the `Folder` row whose path is a library's media directory.

| Kind | Only Jellyfin writes | Differs |
|---|---|---|
| Playlist | the generic keys¹ | — |
| BoxSet | the generic keys¹, `DisplayOrder` | `LibraryFolderIds`: Jellyfin `[]`, Ferrofin `["<the movie's library>"]` |
| UserView | the generic keys¹ | — |
| AggregateFolder (the root) | its whole blob: the generic keys¹, `IsRoot: true`, `PhysicalLocationsList`, `VirtualChildren` | Ferrofin writes no `Data` |
| UserRootFolder; a library location's `Folder` | their whole blob: the generic keys¹ | Ferrofin writes no `Data` |
| CollectionFolder | the generic keys¹ | — |
| Movie | the generic keys¹, `HasSubtitles` (`true`), `AudioFiles`, `SubtitleFiles`, `IsPlaceHolder`, `LinkedAlternateVersions`, `Size` | — |
| Audio | the generic keys¹, `HasLyrics`, `LyricFiles`, `Size` | — |
| The `[RequiresSourceSerialisation]` kinds above | — | neither side writes `Data` |

¹ The generic keys: `DateLastSaved`, `Height`, `Width`, `IsHD`, `IsRoot` on folders, `IsShortcut`, `RemoteTrailers`. Jellyfin writes them for every kind it serialises. Ferrofin writes them only where they hold a value.

Jellyfin's serialiser also fixes the key order, so two blobs with the same keys still differ byte for byte.

`ExtraIds`, which the issue's report named, no longer appears in either side's blobs. Jellyfin 12.4 strips it (`Jellyfin.Server/Migrations/Routines/20260911120000_StripEmbeddedLinkedChildren.cs:34`).

## What each difference means on a return

**Column-backed or computed, so never read from `Data`.**
- `DateLastSaved`, `Height`, `Width` and `Size` are mapped columns (`Jellyfin.Server.Implementations/Item/BaseItemMapper.cs:305-314` written, `:114` and the overlay at `:78-249` read).
- `IsHD` is get-only, `Height >= 720` (`MediaBrowser.Controller/Entities/BaseItem.cs:420`).
- The root's `PhysicalLocationsList` defaults to empty (`MediaBrowser.Controller/Entities/AggregateFolder.cs:37`). `AggregateFolder.RequiresRefresh` (`:116-121`) compares the stored list with the recomputed one, and `CreateResolveArgs` writes it back during refresh (`:162-166`, called at `:82`), so it heals.
- `VirtualChildren` is a get-only bag (`MediaBrowser.Controller/Entities/AggregateFolder.cs:44`), filled at every boot by `CreateRootFolder` (`Emby.Server.Implementations/Library/LibraryManager.cs:1101-1150`).
- `LinkedAlternateVersions` is overlaid from the `LinkedChildren` table (`Jellyfin.Server.Implementations/Item/BaseItemMapper.cs:172-184`). Ferrofin records version merges in that table (`crates/ferrofin-core/src/item_persistence_service.rs:2488-2515`). Its adoption repair moves a 10.11 database's `Data` copy there once (`crates/ferrofin-core/src/adoption_repairs.rs:211` `import_membership_once`, `:311-323`).
- A track's `HasLyrics` is computed from its streams for the DTO (`Emby.Server.Implementations/Dto/DtoService.cs:461`).

**`Data`-only and absent in Ferrofin, but the default is the right value.**
- `IsShortcut` (`MediaBrowser.Controller/Entities/BaseItem.cs:422`) is false except on a shortcut, and Ferrofin writes it for shortcuts (`crates/ferrofin-core/src/library_scan.rs:22788` `overlay_shortcut_data`).
- `RemoteTrailers` defaults to none in the `BaseItem` constructor (`MediaBrowser.Controller/Entities/BaseItem.cs:124`). Ferrofin writes it when a provider supplied trailers (`crates/ferrofin-core/src/item_data.rs:191-219` `merge_remote_trailers`).
- `IsRoot` is false on a library location's `Folder`, which is the default. The user root's constructor sets it true itself (`MediaBrowser.Controller/Entities/UserRootFolder.cs:32`).
- A box set's `DisplayOrder` is `"PremiereDate"` in its constructor (`MediaBrowser.Controller/Entities/Movies/BoxSet.cs:25-27`). Ferrofin reads an absent one as the same value (`crates/ferrofin-core/src/dto_service.rs:3403-3413`).
- A video's `SubtitleFiles` and `AudioFiles`, and a track's `LyricFiles`, default to empty (`MediaBrowser.Controller/Entities/Video.cs:35-42`, `MediaBrowser.Controller/Entities/Audio/Audio.cs:31`).
  - `MediaBrowser.Providers/MediaInfo/ProbeProvider.cs:151-179` triggers a refresh whenever these stored lists of external files differ from the files on disk.
  - The probers then fill them (`MediaBrowser.Providers/MediaInfo/FFProbeVideoInfo.cs:575,595`, `MediaBrowser.Providers/MediaInfo/AudioFileProber.cs:157,584`).
  - `IsPlaceHolder` is false except on a placeholder file.

**`Data`-only and lost: a video's `HasSubtitles`.**
- Jellyfin's prober sets it when any subtitle stream exists, embedded or external (`MediaBrowser.Providers/MediaInfo/FFProbeVideoInfo.cs:270`), and its DTO shows it (`Emby.Server.Implementations/Dto/DtoService.cs:1413-1416`).
- Ferrofin computes it for its own DTOs, but never stores it. The probe's movie has an embedded SRT stream: Jellyfin's blob has `"HasSubtitles":true`, while Ferrofin's has no such key, though its `MediaStreamInfos` holds the subrip stream.
- A return reads it as false. `ProbeProvider.HasChanged` (`:127-182`) re-probes only a changed file, a missing runtime and bitrate (`:184-201`), or a changed list of external files, so the flag stays false until the file changes or is fully refreshed.
- Fix: gl-1632.

**`Data`-only and different: a box set's `LibraryFolderIds`.**
- It decides who sees the box set: an empty list means everyone, otherwise users with one of those libraries (`MediaBrowser.Controller/Entities/Movies/BoxSet.cs:168-190`).
- Jellyfin sets it from the members' libraries, read from the `LinkedChildren` table, when a refresh saves the box set (`MediaBrowser.Providers/BoxSets/BoxSetMetadataService.cs:89-102`, run by `MediaBrowser.Providers/Manager/MetadataService.cs:150`; the members at `MediaBrowser.Controller/Entities/Movies/BoxSet.cs:250-252`).
- Ferrofin stores the members' libraries (`crates/ferrofin-providers/src/metadata_merge.rs:388-423`).
- Jellyfin's own row held `[]`. Probably its refresh ran before the member was linked, but that was not traced. On a returned database the members are linked, so a Jellyfin refresh would compute them as Ferrofin does.

**The `Playlist` keeps everything Jellyfin reads from `Data` only.** `OwnerUserId`, `OpenAccess`, `Shares` and `PlaylistMediaType` have no column (`MediaBrowser.Controller/Playlists/Playlist.cs:41-45,84`). Ferrofin's blob carries all four: `crates/ferrofin-core/src/item_data.rs:998` (`sync_playlist_meta`) keeps the ownership keys in step, and `crates/ferrofin-core/src/collection_manager.rs:1531` writes the media type. A returned playlist keeps its owner, its sharing and its media type.

**The root's `IsRoot` reads back false, and nothing sets it true.**
- **How it reads back.** `CreateRootFolder` loads the stored root at every boot (`Emby.Server.Implementations/Library/LibraryManager.cs:1105-1107`). Only a root it resolves afresh gets `IsRoot` true, from `Emby.Server.Implementations/Library/Resolvers/GenericFolderResolver.cs:25`, and nothing else in 12.4 sets it on a stored root. A Ferrofin-created root has no `Data`, so Jellyfin loads it with `IsRoot` false, and any later save of the root keeps that.
- **Its effect is narrow.** The root's children are the library folders: `AggregateFolder.CreateResolveArgs` flattens two levels (`MediaBrowser.Controller/Entities/AggregateFolder.cs:150-153`). `Emby.Server.Implementations/Library/Resolvers/SpecialFolderResolver.cs` resolves those first (`ResolverPriority.First`): `root/default` as the user root, and each library directory as a `CollectionFolder`.
  - The checks of `args.Parent.IsRoot` in `Emby.Server.Implementations/Library/Resolvers/Movies/MovieResolver.cs:588`, `Emby.Server.Implementations/Library/Resolvers/TV/SeriesResolver.cs:60-99`, `Emby.Server.Implementations/Library/Resolvers/Audio/MusicAlbumResolver.cs:76` and `Emby.Server.Implementations/Library/Resolvers/Audio/MusicArtistResolver.cs:83` therefore only reach a stray directory placed directly under `root/` that is not a library.
- **Which databases have it.** An adopted Jellyfin database keeps Jellyfin's own root blob, so only a database Ferrofin created fresh has the gap. Booting Jellyfin on such a database needs an export into Jellyfin's layout, which Ferrofin does not produce yet, so the return itself was not reproduced.

## Writers behind the differences above

**Ferrofin:**
- **The root:** created without `Data` by `insert_aggregate` (`crates/ferrofin-core/src/aggregate_folder.rs:213`) → `insert_root_folder` (`crates/ferrofin-core/src/item_persistence_service.rs:719`).
- **The playlist's keys:** `crates/ferrofin-core/src/item_data.rs:998`, `crates/ferrofin-core/src/collection_manager.rs:1531`.
- **A box set:** its `DisplayOrder`, only when edited (`crates/ferrofin-api/src/handlers/item_update.rs:560` `set_data_string`, called at `:461`), and its `LibraryFolderIds` (`crates/ferrofin-providers/src/metadata_merge.rs:388-423`).
- **The scan's video keys:** `VideoType`, `AdditionalParts` and `LocalAlternateVersions` (`crates/ferrofin-core/src/library_scan.rs:22860` `overlay_file_facts`), and a shortcut's keys (`:22788`).
- **Trailers:** `crates/ferrofin-core/src/item_data.rs:191-219`.

Ferrofin writes other `Data` keys that the probe's rows did not exercise: `Video3DFormat`, `IsoType`, the editor's `AspectRatio`, `AirDays` and `AirTime`, and a series' `Status`.

**Jellyfin:** the whole item, serialised on every save (`Jellyfin.Server.Implementations/Item/BaseItemMapper.cs:266-268`).

## Follow-ups

- **gl-1632:** store a video's `HasSubtitles` in `Data`, as upstream's prober sets it, so a return keeps it.
- **gl-1629:** write the root `AggregateFolder`'s `Data` as Jellyfin does, `IsRoot` true at least, so a fresh database's root reads back as Jellyfin's does.
- **gl-1630:** write `Data` per kind as Jellyfin's serialiser does: the generic keys, each kind's own fields, and Jellyfin's key order. A returned row is then not rewritten. This issue also covers the kinds not probed here. Allowlisting the difference instead is the owner's call.
