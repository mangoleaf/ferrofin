# Non-database state Jellyfin reads

Evidence for decision D5 (output layout and large-directory strategy) of the "Backwards migration to
Jellyfin" milestone, phase gl-1572. The implementation phase gl-1557 reads this file and does not edit
it. Code is cited at `release/v1.6.0` `1893be47d`; upstream is Jellyfin `v12.4` (`UPSTREAM_TAG`), cited
relative to its checkout.

## Method

One scratch run on a synthetic library, built by the adoption gate's own code
(`gates/adoption/synthetic_media.py`, and `synthetic.py`'s `jellyfin-12.4-fresh` route). That route switches
on chapter-image and trickplay extraction during the scan.

The library:
- 16 videos (10 minutes each, 320x180, one frame per second);
- 6 FLAC tracks in 2 albums, each with a `cover.jpg`;
- 2 photos and 1 EPUB;
- sidecar `.srt` subtitles and one attached font;
- 1,335,114 bytes of media in all.

Every server ran in the official `jellyfin/jellyfin:12.4` image (`JELLYFIN_DATA_DIR=/config`,
`JELLYFIN_CONFIG_DIR=/config/config`, `JELLYFIN_CACHE_DIR=/cache`, `JELLYFIN_LOG_DIR=/config/log`, volumes
`/config` and `/cache`, media at `/media`). Ferrofin was this commit's binary, run in that same image with
`FERROFIN_DATA_DIR=/config` and `FERROFIN_CACHE_DIR=/cache`, so both servers saw the same paths.

| Run | Server | Starts from | Then |
|---|---|---|---|
| fixture | Jellyfin 12.4 | nothing | the gate's seed (wizard, libraries, users, playlists, collections, scan) |
| J | Jellyfin 12.4 | a copy of the fixture | scan, then the exercise |
| F | Ferrofin | a copy of the fixture (adopts it), a fresh `/cache` | scan, then the exercise |
| R | Jellyfin 12.4 | a copy of F's output (the return), a fresh `/cache` | scan, then the exercise |
| S | Jellyfin 12.4 | a copy of R's output | `GET` the two images Ferrofin wrote into its own folder |
| J+, F+ | as J, F | copies of J's and F's output, plus a 17th video (30 seconds) with an embedded SubRip track | scan, then the exercise |

**The exercise:**
- upload a Backdrop to a movie;
- create a playlist and a collection;
- fetch a scaled Primary of every movie and album;
- convert a subtitle stream of every video;
- fetch every attachment.

Every file under program data and the cache was hashed before and after each run and classed by path.
Every stored path was then resolved against the tree.

**Limits:**
- **Remote providers were blocked** (an unreachable proxy). Downloaded artwork and downloaded subtitles
  were never exercised, so their 0 B is untested, not empty.
- **Most of F's tree was written by Jellyfin.** F adopted the fixture, so its chapter images, trickplay
  and most artwork came from the fixture build. Ferrofin wrote only what the size column marks as
  Ferrofin's.
- **A Ferrofin-native install was not run.** One that never came from Jellyfin has all its item artwork in
  Ferrofin's own `{ID}` folders (Divergences, 1), not 2 files of 19. Its by-name artwork is in the by-name
  folders.
- **The Debian layout was read from the gate, not booted.**
- **Each server ran once.**

**Scale.** The sizes are from one-frame-per-second synthetic video, so they show which classes exist, not
real totals. What scales with what:
- trickplay in tile sheets: one thumbnail per interval (10 s by default), 100 thumbnails to a 10x10 sheet,
  so a video has `ceil(runtime / interval / 100)` sheets per width. Every video here fits one sheet (60
  thumbnails for 10 minutes, 3 for the 30-second one), so 36,179 B per video here and 35,480 B for
  Ferrofin's own 30-second set are bytes per sheet of flat synthetic colour, not per minute;
- chapter images with chapter count: 585 B each here;
- artwork with item count.

Non-database program data in F came to 974,709 B, 59 % of it trickplay. That ratio says nothing about a
real library.

## Where each file class lives

"Jellyfin writes it" is what Jellyfin 12.4 does to an existing file of the class:
- **in place:** truncates and rewrites the same file;
- **replaces:** deletes it, then moves a new one in;
- **new files only:** never opens an existing file for writing;
- **skips:** leaves an existing file alone;
- **not written:** never opens it. When an image under the metadata directory is replaced, it deletes the
  previous file (`MediaBrowser.Providers/Manager/ImageSaver.cs:177-190`), which drops only a hard link.

That decides whether a hard link would let Jellyfin change the source (Copy or link, below).

| Class | Ferrofin writes | Jellyfin 12.4 reads and writes | Stored path | Jellyfin writes it | Size in F (files): who wrote them | Needed from the export? |
|---|---|---|---|---|---|---|
| Item artwork, Jellyfin's folder | `metadata/library/{xx}/{idN}/`: read for adopted rows (`crates/ferrofin-core/src/dynamic_images.rs:979-986`); used for chapters, subtitles and lyrics (`crates/ferrofin-core/src/path_manager.rs:245-253`) | `metadata/library/{xx}/{idN}/`, `poster`/`folder`/`backdropN` (`MediaBrowser.Controller/Entities/BaseItem.cs:958-960`, `MediaBrowser.Providers/Manager/ImageSaver.cs:508-509,562`); generated collages too (`Emby.Server.Implementations/Images/BaseDynamicImageProvider.cs:123`) | absolute `BaseItemImageInfos.Path` on both (`Jellyfin.Server.Implementations/Item/BaseItemMapper.cs:456`) | in place (`ImageSaver.cs:277-300`, `FileMode.Create`) | 132,997 B (17): Jellyfin | Yes. Uploaded and downloaded artwork has no other copy |
| Item artwork, Ferrofin's folders | `metadata/library/{ID}/` (upper-case hyphenated), `primary`/`backdropN` (`dynamic_images.rs:988-992`, `crates/ferrofin-providers/src/provider_manager.rs:3346-3348`); older installs also `metadata/library/{id}/`, lower-case (`dynamic_images.rs:993-1001`, read only) | never chooses them. Serves a file a row names: S returned 200, byte-identical, for both | absolute | not written (deletes on replace). Its next save of the type goes to its own folder (R) | 8,869 B (2): Ferrofin | Yes. Rows name these files |
| By-name item folders | `metadata/{People,Genre,MusicGenre,Studio,Year,artists}/…` (`crates/ferrofin-core/src/app_paths.rs:310-332`). Artwork is saved there only while the item's path is in Ferrofin's own directories (`crates/ferrofin-providers/src/image_save.rs:219-241`) | the item's own path (`Emby.Server.Implementations/ServerApplicationPaths.cs:56-83`). Downloaded and uploaded artwork is saved there (`IsSaveLocalMetadataEnabled`, e.g. `MediaBrowser.Controller/Entities/Person.cs:89-91`). Generated collages go to the item's `metadata/library/{xx}/{idN}/` and count in the first row | `BaseItems.Path`: Jellyfin saves `%MetadataPath%/…` (`BaseItemMapper.cs:272,538`); Ferrofin saves the rows it creates resolved (Divergences, 2) | in place (ImageSaver) | 0 B (empty folders) | Yes, as part of `metadata/` |
| Shared album art | `metadata/library/album-covers/{ALBUM-ID}/{sha256}.{ext}` (`crates/ferrofin-core/src/library_scan.rs:9468-9506`, `provider_manager.rs:14`) | no such folder. Its scan points each album back at the album folder's `cover.jpg` (`MediaBrowser.LocalMetadata/Images/LocalImageProvider.cs:31-38`) | absolute | not written (deletes on replace) | 1,650 B (2): Ferrofin | Yes. After R's scan the albums named `cover.jpg` again, but all 6 track rows still named `album-covers` |
| Chapter images | `metadata/library/{xx}/{idN}/chapters/{ticks}_{pos}.jpg` (`path_manager.rs:150-168`) | the same (`Emby.Server.Implementations/Library/PathManager.cs:106-116`) | absolute `Chapters.ImagePath` on both (`Jellyfin.Server.Implementations/Item/ChapterRepository.cs:103`), never expanded on read | in place: `File.Copy(…, true)` (`Emby.Server.Implementations/Chapters/ChapterManager.cs:184`), rare, since the name carries the item's `DateModified` ticks | 46,763 B (80): Jellyfin | Regenerable only where the library extracts chapter images: a missing file is extracted again (`ChapterManager.cs:156-222`). With extraction off, Jellyfin clears each `ImagePath` and then deletes every chapter image no row names (`:219-221`, `:237`, `:296-317`), so the copied files are deleted too |
| Trickplay | `data/trickplay/{xx}/{id-D}/{width} - {tw}x{th}/{n}.jpg` (`app_paths.rs:194-198`, `path_manager.rs:96-104`), or beside the media with `SaveTrickplayWithMedia` | the same (`PathManager.cs:96-102`, `Jellyfin.Server.Implementations/Trickplay/TrickplayManager.cs:848-858`) | none (`TrickplayInfos` holds no path) | replaces: delete, then move (`TrickplayManager.cs:615-626`) | 578,858 B (16): Jellyfin | Regenerable only where the library has trickplay extraction on. Rows whose tiles are missing are dropped (`TrickplayManager.cs:140-165`). With extraction off, Jellyfin deletes the copied tiles and their rows itself (`:304-336`), so for such a library the copy is wasted. Same layout on both. The one set both servers generated (J+, F+) differs in bytes (35,659 B against 35,480 B), and both are valid |
| Subtitle extraction cache | `data/subtitles/{xx}/{id-D}/{index}.{ext}` + `.meta` (`path_manager.rs:52-56`) | the same (`PathManager.cs:39,76-93`) | none | in place (`MediaBrowser.MediaEncoding/Subtitles/SubtitleEncoder.cs:422,964`) | 131 B (2): Ferrofin, in F+ | Regenerable, on demand |
| Downloaded subtitles | beside the media by default, else the item's `metadata/library/{xx}/{idN}/` (`crates/ferrofin-core/src/subtitle_manager.rs:228-249`) | the same (`MediaBrowser.Providers/Subtitles/SubtitleManager.cs:229-237`) | `MediaStreamInfos.Path`: Jellyfin saves it reversed (`Jellyfin.Server.Implementations/Item/MediaStreamRepository.cs:209`) and expands it on read (`:86`); Ferrofin saves it absolute (`subtitle_manager.rs:252-278`) | new files only: `FileMode.CreateNew` with a counter (`SubtitleManager.cs:266-280`) | not exercised | Yes when in `metadata/`, since there is no other copy |
| Attachment cache | `data/attachments/{xx}/{id-D}/{file}` (`path_manager.rs:57-61`) | the same (`PathManager.cs:41,62-73`) | none (`AttachmentStreamInfos` holds the file name only) | skips an existing file (`MediaBrowser.MediaEncoding/Attachments/AttachmentExtractor.cs:153`); R fetched it and left it unchanged | 28 B (1): Ferrofin | Regenerable, on demand |
| Playlist folders | `data/playlists/<name>/` (`crates/ferrofin-core/src/aggregate_folder.rs:124-128`, `crates/ferrofin-core/src/collection_manager.rs:1472-1478`); no `playlist.xml` (Divergences, 3) | `data/playlists/<name>/playlist.xml` (`Emby.Server.Implementations/Library/LibraryManager.cs:1117`, `MediaBrowser.LocalMetadata/Savers/PlaylistXmlSaver.cs:22,71`) | `%AppDataPath%/playlists/<name>` on both | in place (`MediaBrowser.LocalMetadata/Savers/BaseXmlSaver.cs:103-110`). In R it wrote the missing `playlist.xml` of the Ferrofin-made playlist on its first refresh | 2,938 B (4): Jellyfin; Ferrofin's own playlist folder is empty | Yes, the folder is the item's path |
| Collection folders | `data/collections/<name> [boxset]/`, no `collection.xml` (`collection_manager.rs:404-419`) | `data/collections/<name> [boxset]/collection.xml` (`Emby.Server.Implementations/Collections/CollectionManager.cs:123,165`, `MediaBrowser.LocalMetadata/Savers/BoxSetXmlSaver.cs:48`) | `%AppDataPath%/collections/…` on both | in place (`BaseXmlSaver.cs:103-110`) | 941 B (2): Jellyfin | Yes, as above |
| Library definitions | `root/default/<library>/`. `.mblink` and `<type>.collection` are kept. `options.json` sits beside the adopted `options.xml`, which is never written again (`crates/ferrofin-core/src/virtual_folder_manager.rs:56-64,1313-1328`) | `root/default/<library>/options.xml`, `*.mblink`, `<type>.collection` (`ServerApplicationPaths.cs:34-35`, `MediaBrowser.Controller/Entities/CollectionFolder.cs:164,182-188`, `LibraryManager.cs:4154`) | `BaseItems.Path` absolute (`/config/root/default/…`) on both; paths inside `.mblink` and `options.xml` tokenised | in place (`Emby.Server.Implementations/Serialization/MyXmlSerializer.cs:72-74`) | 81,489 B (28): 21 Jellyfin, the 7 `options.json` (34,731 B) Ferrofin | Yes. `options.xml` must come from `options.json` (gl-1555) |
| Configuration | `config/system.json` and `config/users/named/*.json` beside the adopted XML (`crates/ferrofin-core/src/configuration_manager.rs:295-326`) | `config/*.xml` (`Emby.Server.Implementations/AppBase/BaseApplicationPaths.cs:70`) | `ImageInfos.Path` (profile images) absolute on both, never expanded | in place (`Emby.Server.Implementations/AppBase/BaseConfigurationManager.cs:168,349` through `MyXmlSerializer`) | 21,148 B: XML Jellyfin, JSON and `system_id` Ferrofin | Yes, as XML (gl-1554) |
| Plugin settings | each plugin's state under `config/plugins/` (`apps/ferrofin-server/src/state.rs:2049-2051`). Jellyfin's `plugins/configurations/LDAP-Auth.xml` is imported once (`crates/ferrofin-extensions/src/ldap_auth/adoption.rs:8`) | `plugins/configurations/<plugin>.xml` (`BaseApplicationPaths.cs:58-61`) | — | in place | 1,248 B (2): Jellyfin (TMDb and MusicBrainz), untouched by Ferrofin | Yes, as XML. Which settings carry over is D7's question (gl-1571, gl-1556) |
| Server id | `config/system_id`, which wins over `data/device.txt`. Ferrofin never writes `device.txt` (`apps/ferrofin-server/src/state.rs:3393-3426`) | `data/device.txt` (`Emby.Server.Implementations/Devices/DeviceId.cs:38`) | — | in place, only when the file is missing or invalid (`DeviceId.cs:70-101`) | 32 B and 35 B: the same id. Jellyfin writes `device.txt` with a 3-byte UTF-8 BOM, which Ferrofin strips on read. They are equal here only because F adopted the fixture | Yes, as a value: the export writes `data/device.txt` from `system_id`, so paired clients still know the server. A Ferrofin-native install has no `device.txt` |
| Database and snapshots | `data/jellyfin.db` adopted in place (`apps/ferrofin-server/src/config.rs:1531-1537`), plus `jellyfin.db.pre-ferrofin` and `jellyfin.db.pre-0044` | `data/jellyfin.db` (`src/Jellyfin.Database/Jellyfin.Database.Providers.Sqlite/SqliteDatabaseProvider.cs:66`) | — | in place | 1,224,704 B + 1,716,224 B of snapshots | The database yes (gl-1548); the snapshots are Ferrofin's |
| Other program data | `task_results.json` at the program-data root (`apps/ferrofin-server/src/state.rs:1852`); `data/splashscreen.png` (`crates/ferrofin-drawing/src/image_encoder.rs:1420`); `data/backups`; `log/log.<date>.log` | `data/ScheduledTasks/*.js` (`Emby.Server.Implementations/ScheduledTasks/ScheduledTaskWorker.cs:421`); `data/splashscreen.png` (`src/Jellyfin.Drawing.Skia/SkiaEncoder.cs:795`) | — | in place: the splashscreen after every scan, task results after each task | under 0.2 MB | No. Each server regenerates its own |
| Cache | `/cache/images/…` (`apps/ferrofin-server/src/state.rs:614-616`), `temp`, `remote-images`, `extensions`, `transcodes` | `/cache/images/…` (`BaseApplicationPaths.cs:55`), `fontconfig`, `transcodes` | — | in place | 3,538 B: Ferrofin | No. R started on an empty `/cache` with no error |

## Stored paths

- **Jellyfin saves two path columns through `ReverseVirtualPath`:** `BaseItems.Path` (`BaseItemMapper.cs:272,538`)
  and `MediaStreamInfos.Path` (`MediaStreamRepository.cs:209`). `ReverseVirtualPath` turns `{data}` into
  `%AppDataPath%` and the metadata directory into `%MetadataPath%` (`Emby.Server.Implementations/ApplicationHost.cs:286-304`).
- **It expands tokens on read in three columns:** `BaseItems.Path` (`BaseItemMapper.cs:82`),
  `BaseItemImageInfos.Path` (`BaseItemMapper.cs:435`) and `MediaStreamInfos.Path` (`MediaStreamRepository.cs:86`).
- **Two columns are absolute and never expanded:** `Chapters.ImagePath` and `ImageInfos.Path`.
- **The root and library folder rows** (`/config/root…`) sit under program data, not under `{data}`, so no
  token covers them.
- **Every path in these four columns resolved** in J, F and R: `BaseItemImageInfos.Path`, `Chapters.ImagePath`,
  `ImageInfos.Path`, and `BaseItems.Path` under program data. In F that was 54, 80, 7 and 31 rows, none
  missing. `MediaStreamInfos.Path` held no program-data path to check.
- **Jellyfin has no repair for a moved program-data directory.** It resets only the root folders' own paths
  (the note at `crates/ferrofin-core/src/adoption_repairs.rs:6985-6991`). Ferrofin's boot repair
  `rebase_program_data_paths` (`adoption_repairs.rs:7046-7078`) rewrites the five Jellyfin-owned path columns
  in `PATH_COLUMNS` (`adoption_repairs.rs:6993-7001`). The other two columns there are Ferrofin's.
- **So an export Jellyfin reads at a directory other than the one the stored paths name has two routes:**
  - rewrite the five columns onto the target directory, the inverse of that rebase;
  - write `%AppDataPath%` / `%MetadataPath%` into the three columns Jellyfin expands, which makes them
    layout-independent. `Chapters.ImagePath`, `ImageInfos.Path` and the root and library folder rows still
    need the real target prefix.
- **The stored prefix depends on the layout:**
  - Jellyfin's docker image: program data at `/config`, configuration at `/config/config`;
  - Ferrofin's own image: `FERROFIN_DATA_DIR=/data` (`Dockerfile:94`). A Jellyfin `/config` tree adopted
    there is rebased onto `/data` at the first boot;
  - the Debian package: `/var/lib/ferrofin`.

  So **even a docker-only export needs one of the two routes** for every source that did not already run at
  `/config`.

## Debian layout

The adoption gate's Debian fixture (`gates/adoption/synthetic.py:63-71`) runs Jellyfin with:
- `JELLYFIN_DATA_DIR=/var/lib/jellyfin`
- `JELLYFIN_CONFIG_DIR=/etc/jellyfin`
- `JELLYFIN_CACHE_DIR=/var/cache/jellyfin`
- `JELLYFIN_LOG_DIR=/var/log/jellyfin`

The program data then holds `data/`, `metadata/`, `root/` and `plugins/` as in the docker layout. The
configuration, with the profile images under `users/`, lives apart. `scripts/migrate-jellyfin.sh:17-25`
copies such an install into Ferrofin, and `gates/adoption/program_data_move.py` checks the rebase that
follows. These paths are the Jellyfin packages' defaults as the gate models them. Jellyfin's repository
ships no packaging.

**What option A means for a Debian-package Jellyfin.** A docker-layout export names `/config` in its
stored paths and keeps its configuration in `/config/config`. A Jellyfin installed from the Debian package
reads `/var/lib/jellyfin` and `/etc/jellyfin`. It cannot use the export as written unless the operator
either runs Jellyfin's docker image or points the package at the export placed at `/config`. A Debian
export (option B) is the docker export with the configuration written apart and another target prefix.
It needs no other file class. That is an estimate from the code: the Debian layout was not booted here.

`scripts/migrate-jellyfin.sh` copies no `data/trickplay`, going into Ferrofin. It costs a rebuild or the
loss of every trickplay set (the Trickplay row). Filed as gl-1576.

## Copy or link

Jellyfin writes in place to the artwork in its own and the by-name folders, and to the chapter-image,
subtitle-cache, playlist, collection, library, configuration and plugin-settings classes ("Jellyfin writes
it" column). A hard link of any of them would let Jellyfin change
the Ferrofin source, which the export promises never to change.

On the code's evidence, **five classes are link-safe:**
- **trickplay**, which Jellyfin replaces by delete-then-move, so it only drops its own link;
- **the attachment cache**, which Jellyfin skips when the file exists;
- **downloaded subtitles**, which Jellyfin writes only as new files;
- **Ferrofin's own artwork folders** and **`album-covers`**, which Jellyfin never writes and only deletes on
  replace. For a Ferrofin-native install, those folders hold its item artwork.

**What R changed:**
- in place: `data/jellyfin.db`, `data/splashscreen.png`, `data/ScheduledTasks/*.js` and its log;
- added: new files under `metadata/library/`, `data/playlists/`, `data/collections/` and `/cache`;
- left alone: every artwork, chapter-image, trickplay and attachment file and every `root/default` file
  it was given, and the configuration. Its input held no subtitle-cache file.

One run is not a guarantee. A "replace existing images" refresh rewrites the adopted `poster.*` files in
place.

## Divergences found

None was seen to block a return: every path resolved in R, and S served Ferrofin's own files.

1. **Ferrofin's own artwork folders.** Filed as gl-1575.
   - New artwork goes to `metadata/library/{ID}/primary.*` (`dynamic_images.rs:988-992`,
     `provider_manager.rs:3346-3348`). Jellyfin uses `metadata/library/{xx}/{idN}/poster.*`
     (`BaseItem.cs:958-960`, `ImageSaver.cs:508-509,562`).
   - Jellyfin serves Ferrofin's file (S).
   - Jellyfin's next save of that image type goes to its own folder. In R, `backdrop2.png` landed in
     `3d/3dfe…`, beside Ferrofin's `3DFE…/backdrop1.png`.
   - Deleting the item in Jellyfin removes only its own folder (`LibraryManager.cs:725-729`).
2. **By-name item paths stored resolved.** This is gl-1209, with this run's evidence added.
   - Ferrofin writes the `BaseItems.Path` of the program-data items it creates resolved. F and R hold
     `Year` as `/config/metadata/Year/2020` (`crates/ferrofin-core/src/years.rs:92`).
   - Upstream saves every item path through `ReverseVirtualPath` (`%MetadataPath%/Year/2020`; the oracle at
     `crates/ferrofin-core/src/item_type_lookup.rs:712-714`).
   - In Ferrofin, only the API-created playlist and box-set rows are reversed (`collection_manager.rs:1478`).
   - The export's path handling covers these rows either way.
3. **No `playlist.xml` or `collection.xml`** for playlists and box sets made in Ferrofin. This is known:
   gl-341, `crates/ferrofin-providers/src/local_xml.rs:17-30`. Membership is in `LinkedChildren`, which
   Jellyfin 12.4 reads. In R, Jellyfin wrote the missing `playlist.xml` itself. R shows nothing for a
   Ferrofin-made box set, because R's exercise made one of the same name.
4. **`album-covers`.** Its parity question is gl-1564. For the export, it means the tree must be copied:
   Jellyfin re-resolves the album, not its tracks. One cover file is shared by an album's tracks. When
   Jellyfin replaces one track's Primary, it deletes the previous file (`ImageSaver.cs:177-190`), which the
   other tracks' rows still name. That is inferred from the code, not seen in R.

## For D5

**Updated recommendation: A, amended.** Docker layout. Copy everything by default, with no hard-link
option over the whole tree. Handle the stored paths. Preflight reports the size of each class and the free
space. Concretely:

- **Copy the first three groups below by default, and skip the fourth.** Each class falls into one of four
  groups:
  - **Needed:**
    - the database;
    - `metadata/`: uploads and downloads, `album-covers`, Ferrofin's `{ID}` and `{id}` folders;
    - `data/playlists`, `data/collections` and `root/default`;
    - the configuration and plugin settings, as XML;
    - the server id, written as `data/device.txt` from `system_id`.
  - **Regenerable, at a CPU cost and only where the library extracts them:** `data/trickplay` and the
    chapter images. Otherwise Jellyfin deletes the copied trickplay and its rows, and clears each chapter's
    image path and deletes the copied chapter images (the chapter rows stay).
  - **Regenerable on demand:** `data/subtitles` and `data/attachments`.
  - **Not needed:**
    - `/cache`;
    - what each server regenerates: `data/splashscreen.png`, `data/ScheduledTasks`, `log/`;
    - Ferrofin's own files: `*.json` configuration once exported as XML, `options.json`,
      `task_results.json`, and the database snapshots, each a full database copy.

  A synthetic library cannot rank the classes by size. Preflight's per-class sizes rank them on the real
  one.
- **No hard-link flag over the tree.** Jellyfin truncates and rewrites images and XML in place.
  - **Option:** a link limited to trickplay is safe, since Jellyfin replaces it by delete-then-move, and
    trickplay is likely the largest of the link-safe classes. I do not recommend it for the first version.
  - **Untested proposal:** a reflink copy (`FICLONE`, on the same btrfs or XFS filesystem) shares blocks
    copy-on-write with no shared-write hazard. It could be the copy method wherever it works, with no flag.
- **Handle the stored paths.** Either rewrite the five columns onto `/config`, the inverse of
  `rebase_program_data_paths`, or write tokens into the three columns Jellyfin expands and rewrite only
  `Chapters.ImagePath`, `ImageInfos.Path` and the root and library folder rows. Both are needed even for
  docker, because Ferrofin's image runs at `/data` and the Debian package at `/var/lib/ferrofin`.
- **Debian layout (option B).** Once the path handling exists, it adds a split configuration directory and
  another target prefix. The gate already has a Debian fixture. A Debian-package Jellyfin user cannot use an
  A export without moving to docker or to `/config`, so the owner trades that against B's extra route.
