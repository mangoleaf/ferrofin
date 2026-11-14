# Album covers: Ferrofin's shared `album-covers` copy against Jellyfin's album image

Investigation gl-1564. The question: why an album's image row points into Ferrofin's `metadata/library/album-covers/`, where Jellyfin's points at the image in the album folder, and whether a return to Jellyfin needs a file Ferrofin generated. Upstream citations are to Jellyfin `v12.4`. Ferrofin citations are to `release/v1.6.0` @ `978bea505`.

**Verdict:**
- **What differs.** With a library's "Save artwork into media folders" off, the default, Ferrofin stores every album's Primary as a content-addressed copy under `{metadata}/library/album-covers/{album}/`. Jellyfin stores a folder image in place, and copies an embedded-only cover into the album's own metadata folder.
- **Why.** The shared copy was added to stop extracting the same cover once per track, and every track names it too (gl-692). The album row took it so that the scan's two passes stop rewriting each other's row.
- **Folder albums heal.** Jellyfin's first scan after a return points the row at `folder.jpg`, as the report this investigation came from saw.
- **Embedded-only albums keep needing Ferrofin's file.** After a return they name the shared copy for good. Without it, a scan leaves them with no cover until a full refresh.
- **With "Save artwork into media folders" on, it differs the other way.** Ferrofin writes an embedded-only album's cover into the album's media folder as `folder.jpg`, which Jellyfin never does.
- **The playlist collage needs nothing beyond gl-1575.** Its changed `Id` and BlurHash after Jellyfin saves it are Jellyfin's own row rewrite and its re-measure, whose difference from Ferrofin's value the owner accepted.
- **The fix, per decision D8 of the "Backwards migration to Jellyfin" milestone (match upstream unless the owner accepts a divergence), is gl-1633.**

## Reproduction

The same steps on two fresh servers. In the commands and paths below, `{idN}` is an item's id without dashes, `{xx}` its first two characters, and `{ID}` the upper-case hyphenated id.

**The servers:**
- **Ferrofin**, built from `release/v1.6.0` @ `978bea505`: `FERROFIN_ADMIN_PASSWORD=pw ferrofin-server --data-dir <data> --bind 127.0.0.1 --port <port>`. Its database is `<data>/ferrofin.db`.
- **Jellyfin**: `docker run -d --user "$(id -u):$(id -g)" -p 127.0.0.1:<port>:8096 -v <config>:/config -v <cache>:/cache -v <media>:/media:ro jellyfin/jellyfin:12.4`. Its database is `<config>/data/jellyfin.db`. Finish its wizard first: `POST /Startup/Configuration` with `{"UICulture":"en-US","MetadataCountryCode":"US","PreferredMetadataLanguage":"en"}`, `GET /Startup/User`, `POST /Startup/User` with `{"Name":"admin","Password":"pw"}`, then `POST /Startup/Complete`.

Send every request, the sign-in included, with `Authorization: MediaBrowser Client="probe", Device="probe", DeviceId="probe", Version="1"`. Sign in to either with `POST /Users/AuthenticateByName` and `{"Username":"admin","Pw":"pw"}`, then add `, Token="<AccessToken>"` to the header.

The media, made fresh for each run (a run with "Save artwork into media folders" on writes into it), with the embedded cover kept in a work directory `<work>` outside the library:

```sh
mkdir -p "<media>/music/Test Artist/Folder Album" "<media>/music/Test Artist/Embedded Album"
ffmpeg -y -f lavfi -i color=blue:size=300x300 -frames:v 1 "<work>/cover.png"
cd "<media>/music/Test Artist"
ffmpeg -y -f lavfi -i color=red:size=300x300 -frames:v 1 "Folder Album/folder.jpg"
ffmpeg -y -f lavfi -i sine=frequency=440:duration=2 -metadata title="Folder Track" -metadata artist="Test Artist" -metadata album="Folder Album" -metadata track=1 "Folder Album/01 Folder Track.flac"
ffmpeg -y -f lavfi -i sine=frequency=550:duration=2 -i "<work>/cover.png" -map 0 -map 1 -c:a flac -c:v png -disposition:v attached_pic -metadata title="Embedded Track" -metadata artist="Test Artist" -metadata album="Embedded Album" -metadata track=1 "Embedded Album/01 Embedded Track.flac"
```

1. `POST /Library/VirtualFolders?name=Music&collectionType=music&refreshLibrary=true` with `{"LibraryOptions":{"PathInfos":[{"Path":"<media>/music"}]}}`, where `<media>` is `/media` for Jellyfin.
2. Wait until `GET /Items?Recursive=true&IncludeItemTypes=Audio` lists both tracks, then 20 s more.
3. `POST /Playlists` with `{"Name":"Probe mix","Ids":["<track 1>","<track 2>"],"UserId":"<user>","MediaType":"Audio"}`. The tracks are the two `Id`s step 2's listing returns, and `<user>` is the `Id` of `GET /Users/Me`.
4. Wait 30 s and stop the server.

The Primary image rows, on either database:

```sql
SELECT b.Type, b.Name, i.Path, i.Width, i.Height, i.BlurHash
FROM BaseItemImageInfos AS i JOIN BaseItems AS b ON b.Id = i.ItemId
WHERE i.ImageType = 0
ORDER BY b.Type, b.Name;
```

They are, with `{album}` the album's id and each shared file named by its SHA-256:

| Item | Jellyfin 12.4 | Ferrofin |
|---|---|---|
| Folder Album | `/media/music/Test Artist/Folder Album/folder.jpg` | `metadata/library/album-covers/{album}/4be14f…d5.jpg`, a copy of `folder.jpg` |
| Embedded Album | `metadata/library/f5/{idN}/folder.jpg` | `metadata/library/album-covers/{album}/fd90e8…7b.jpg` |
| Folder Track | none | the Folder Album's shared copy |
| Embedded Track | `metadata/library/b6/{idN}/poster.jpg`, its extracted cover | the Embedded Album's shared copy |
| Probe mix (playlist) | `metadata/library/af/{idN}/poster.png`, 600×600 | `metadata/library/{ID}/primary.png`, 600×600 |
| the root folder | none | `metadata/library/{ID}/primary.jpg`, a copy of the Embedded Track's cover |

Every row on both sides has its size and a BlurHash.

**With "Save artwork into media folders" on.** The same run on Ferrofin, with `"SaveLocalMetadata":true` added to `LibraryOptions`, gives:
- **Both albums:** the row names a `folder.jpg` in the album's media folder.
- **Embedded Album:** that `folder.jpg` is a new file, 1.3 kB, the extracted cover, written there by Ferrofin.
- **The tracks:** they still name the shared copies.

Jellyfin keeps an embedded-only album's copy in its metadata folder whatever this setting says, because a dynamic image provider saves with `saveLocallyWithMedia: false` (`Emby.Server.Implementations/Images/BaseDynamicImageProvider.cs:123`).

## The writers

**Ferrofin.**
- **The shared copy.** `publish_album_cover` (`crates/ferrofin-core/src/library_scan.rs:9460-9536`) hashes the album's source image and writes it to `{metadata}/library/album-covers/{album}/{sha256}.{ext}`. The source is a folder image from the album pass (`:9430-9439`), or a track's extracted embedded cover (`album_track_cover`, `:9540-9581`).
- **The album row.** The walk replaces the album's Primary with that copy (`:13517-13526`), and so does `inherit_album_cover` (`:13284`). The comment at `:13517-13520` gives the reason: storing the source made the walk and the album pass rewrite each other's row on every scan.
- **With "Save artwork into media folders" on.** `inherit_album_cover`'s image saver publishes the copy into the album's media folder (`crates/ferrofin-providers/src/image_save.rs:291`), naming an album's Primary `folder` (`crates/ferrofin-providers/src/image_save/policy.rs:218-221`).
- **The tracks.** Each track gets the same copy, from `album_track_cover` (`:9547-9549`) and from the track loop of `inherit_album_cover` (`:13363-13382`). That is gl-692.
- **Never removed.** The copies are kept when nothing names them any more (`:9444-9446`, `crates/ferrofin-providers/src/provider_manager.rs:11-14`).
  - Replacing or deleting an image skips the folder (`crates/ferrofin-providers/src/provider_manager.rs:3405-3407`, `:5112-5117`; `crates/ferrofin-providers/src/image_save.rs:389-392`).
  - Deleting an item removes only the item's own folders (`crates/ferrofin-core/src/item_data_cleanup.rs:93-110`), so `album-covers/` is never reached.
- **History.** The shared copy came with the squash commit `70baaeb34` (2026-09-17), in its part "perf: share music artwork by album during library scans": per-track extraction had duplicated every album's cover. The album row took the copy with per-item change detection (`e9550cf5b`, 2026-09-24).

**Jellyfin.**
- **A folder image.**
  - `MediaBrowser.LocalMetadata/Images/LocalImageProvider.cs:107-128` lists the images in the album's folder, and `:262-266` gives an album the names that put `folder` first (`:30-38`).
  - `MediaBrowser.Providers/Manager/ItemImageProvider.cs:440-461` (`MergeImages`) points the Primary at the file in place.
- **Only an embedded cover.**
  - **The source.** `Emby.Server.Implementations/Images/MusicAlbumImageProvider.cs:22-28` takes the album's first child that is not a folder and has a Primary, ordered by sort name (`Emby.Server.Implementations/Images/BaseFolderImageProvider.cs:29-43`).
  - **The copy.** `Emby.Server.Implementations/Images/BaseDynamicImageProvider.cs:305-323` copies that child's file.
  - **The save.** `:123` saves it through `ProviderManager.SaveImage` as `folder.<ext>` in the album's internal metadata folder (`MediaBrowser.Providers/Manager/ImageSaver.cs:503-509,555-563`). That folder is `library/{xx}/{idN}` (`MediaBrowser.Controller/Entities/BaseItem.cs:951-961`).
- **The track's own cover.** `MediaBrowser.Providers/MediaInfo/AudioImageProvider.cs:77-107` extracts it, and `MediaBrowser.Providers/Manager/ItemImageProvider.cs:241` saves it as the track's `poster.jpg`. A track without an embedded cover has no image row.
- **What a dynamic provider leaves alone.** `BaseDynamicImageProvider` replaces an item's image only while it lies inside the item's internal metadata folder (`:90-93` on fetch, `:266-291` on the change check). Anything elsewhere counts as the user's and is kept. An album's single-image copy is also never redrawn by date (`BaseFolderImageProvider.cs:56-64`).

## What a return does with these rows

Booting Jellyfin on a database Ferrofin wrote needs an export into Jellyfin's layout (gl-1557), so the return was simulated.
1. **The rows.** On a copy of the Jellyfin probe's `/config`, each album's, track's and playlist's Primary row took Ferrofin's values: the path, rebased onto `/config/metadata/library/`, the size, the BlurHash and the date.
2. **The files.** Jellyfin's own copies of those images were moved out. Variant A copied Ferrofin's files in at those paths, keeping their modification times; variant B left them out, as an export that drops `album-covers/` would.
3. **The run.** Jellyfin 12.4 booted on each variant, each image was fetched over HTTP, the library was scanned (`POST /Library/Refresh`, then wait for `RefreshLibrary` to go idle), and one entry was removed from the playlist.

| Item | A: after boot | A: after the scan | B: after boot | B: after the scan |
|---|---|---|---|---|
| Folder Album | 200, the shared copy | 200, `folder.jpg` in place | 404 | 200, `folder.jpg` in place |
| Embedded Album | 200, the shared copy | 200, still the shared copy | 404 | 404, no image row |
| Folder Track | 200, the shared copy | 200, still the shared copy | 404 | 404, no image row (Jellyfin's own state) |
| Embedded Track | 200, the shared copy | 200, still the shared copy | 404 | 404, no image row |
| Probe mix | 200, Ferrofin's collage | 200, Ferrofin's collage, BlurHash and date as Ferrofin wrote them | 404 | 200, a new collage in Jellyfin's folder |

In variant B, a full refresh of the Embedded Album (`POST /Items/{id}/Refresh?Recursive=true&MetadataRefreshMode=FullRefresh&ImageRefreshMode=FullRefresh`) restored both covers, as Jellyfin's own `{xx}/{idN}/folder.jpg` and `poster.jpg`.

So:
- **A folder album heals on Jellyfin's first scan.** The shared copy is needed only until then.
- **An embedded-only album keeps naming the shared copy for good.** It lies outside the album's internal metadata folder, so `MusicAlbumImageProvider` leaves it alone. The file is therefore needed for as long as the database lives. Without it, the album and its track have no cover until a full refresh.
- **The folder is left behind.** Neither side cleans `album-covers/`. Ferrofin keeps it by design. Jellyfin deletes only an item's own metadata folders (`Emby.Server.Implementations/Library/LibraryManager.cs:370`, `GetInternalMetadataPaths` at `:725-729`).

**The playlist collage.**
- **The scan left it alone.** In variant A, the scan kept the row's BlurHash and date as Ferrofin wrote them. Whether the scan alone changed the row's `Id` was not recorded.
- **The playlist edit re-measured it.** A save for an image update or a bigger change re-measures every image of the item (`Emby.Server.Implementations/Library/LibraryManager.cs:2802`, `:2556-2562`). A plain scan re-measures only a local image whose width, height or BlurHash is missing, or whose date differs from its file's by more than a second (`:2532-2545`).
- **The values agree.** The re-measure gave `e~LZ}sjufQjufQ2Na}fQa}fQWrfQfQfQfQ,SjsfQjsfQfQfQfQfQfQ`, the same BlurHash Jellyfin computes for its own collage of the same two tracks.
- **The `Id` is Jellyfin's churn.** Every Jellyfin save deletes an item's image rows and adds them back with new ids (`Jellyfin.Server.Implementations/Item/ItemPersistenceService.cs:478-485,352`; `Id = Guid.NewGuid()` at `Jellyfin.Server.Implementations/Item/BaseItemMapper.cs:452-455`). Its own rows churn the same way.
- **The BlurHash difference is accepted.** Ferrofin's stored value differs from Jellyfin's re-measure. The owner decided that matching Jellyfin's hash strings is not a goal (gl-1058, 2026-10-31). The cause here was not isolated. gl-1058's double scaling affects neither this PNG nor the probe's flat-colour `folder.jpg`, for which Jellyfin stores a DC of `(254,0,0)` and Ferrofin `(255,1,1)`. The likely sources are JPEG decoding for `folder.jpg`, and for the collage the resize gl-1058 also names: Ferrofin's Triangle fit against Jellyfin's nearest-neighbour sample.
- **The folder is gl-1575's.** The collage's location and stem, `{ID}/primary.png` against `{xx}/{idN}/poster.png`, are what remain.

**The root folder's image differs only in timing and folder.** Ferrofin draws it on its first scan (`crates/ferrofin-core/src/dynamic_images.rs:627`, `copy_single_image`). Jellyfin's `FolderImageProvider` gave its own root the same file on its second scan, in variant A, at `{xx}/{idN}/poster.jpg`. The folder is gl-1575's.

## Follow-ups

- **gl-1633:** store an album's own Primary as Jellyfin does. That is a folder image in place, or else a copy of the first child's cover as `folder.<ext>` in the album's own metadata folder, whatever "Save artwork into media folders" says. A returned album then no longer depends on `album-covers/`.
- **gl-692** (open, awaiting an owner decision) covers the tracks that take the album's copy. `album-covers/` falls out of use only if its fix also stops tracks naming the shared copy.
- **gl-1575** (open) covers the per-item art folder, `{ID}` against `{xx}/{idN}`, for the collage, the root's image and gl-1633's copy.
- **gl-1557** (open) will make an export carry what the "Backwards migration to Jellyfin" milestone settles in its still-open decision D5. Until gl-1633 lands, an export that leaves `album-covers/` out costs every embedded-only album its cover.
