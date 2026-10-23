# Upgrading Ferrofin

Operator-facing notes for upgrades that need a manual step or change behaviour in a
way the release notes do not make obvious. Newest first. `CHANGELOG.md` lists *what*
changed; this file says *what you have to do about it*.

Ferrofin's own database upgrades in place: start the new version against the same
data directory and its migrations run on boot. Back up the data directory before a
major-version upgrade.

## Unreleased — extras take their owner's metadata; refresh once to fill existing ones

Trailers, featurettes and other extras now take their movie's or series' genres,
studios, overview, ratings, year and premiere date, as in Jellyfin. A scan gives them
the year and premiere date every time. It gives them the rest when it finds an extra
new, renamed or gone, or when the refresh is a full one. Extras already in your
libraries get only the dates from an ordinary scan.

To fill the rest for existing extras, refresh each movie or TV library once with
**Search for missing metadata** (Dashboard → Libraries → the library's menu → Refresh
metadata), or refresh single items the same way. That is a full refresh, which is all
the copy needs; **Replace all metadata** also works but re-fetches everything. New
extras get everything on the scan that finds them.

## Unreleased — TheAudioDB descriptions return, in the library's language

TheAudioDB stopped filling its English-suffixed description fields, and earlier versions
read only those, so music artists and albums got no description from it. Ferrofin now reads
the description in the item's **preferred metadata language** (its own, else the
library's, else the server's) when TheAudioDB has one, and the English text otherwise.
That choice is limited to the languages TheAudioDB translates (German, English, Spanish,
French, Hebrew, Hungarian, Italian, Japanese, Dutch, Norwegian, Polish, Portuguese,
Russian, Swedish and Chinese).

Descriptions appear on the next refresh that asks the providers.
- **Search for missing metadata** fills only items with no description.
- **Replace all metadata** replaces an existing one, and so does a refresh that is due
  under the library's automatic refresh interval.

**Music artists with no saved choices ask TheAudioDB again**, as Jellyfin 12.2 does. This
covers artists known only by name, and artists in a library that never saved **Music
Artists** downloader choices. The 1.3.0 note below says the server-wide options turn
TheAudioDB off for these artists. For metadata that no longer holds: a music artist with no
saved choices may use every metadata downloader, whatever the server-wide
`MetadataOptions` disable. Image fetchers and albums still follow the server-wide options,
which ship with TheAudioDB metadata off for albums.

To stop TheAudioDB for a library's artists, open **Dashboard → Libraries** and choose
**Manage library**. Under **Music Artists**, TheAudioDB shows unticked if the library never
saved choices (save once to keep that) and ticked if it did (untick it, then save). Artists known only by name have no library, so there
is no setting that stops TheAudioDB for them.

## Unreleased — rows of deleted users are removed, once

A long-lived Jellyfin database can still hold rows of users deleted long ago (their
permissions, preferences, play state, devices and display preferences) and ancestry rows
of items that no longer exist. Jellyfin 12.1 and later start with them; earlier Ferrofin
versions refused to open such a database ("foreign-key violation(s)"), on adoption and on
every boot after.

On first boot migration `0044` deletes those rows, as Jellyfin would have when the user or
item was deleted, and logs how many it removed from each table. It snapshots the file to
`jellyfin.db.pre-0044` first, unless an earlier table-rebuilding migration (`0007`,
`0032`) runs in the same boot and takes the snapshot. Adoption also keeps its
`jellyfin.db.pre-ferrofin` copy, as always. Nobody can reach the deleted
rows, so nothing visible changes. A database the earlier version refused now opens.

## Unreleased — items whose owner is gone are removed, once

A Jellyfin 12.x database can hold an item whose owning item is gone, typically an extra
(a trailer, a featurette). Jellyfin 12.2 starts with it and keeps the row (an extra stays
visible in default queries); earlier Ferrofin versions refused to open such a database
("foreign-key violation(s)"), on adoption and on every boot after.

On first boot migration `0045` hands such an item to the placeholder item, as `0032` does
for a 10.11 database, and the start-up repair then deletes it with what hangs under it
(streams, images, links), logging how many items it removed. Play state recorded for it
moves to the placeholder item, as in Jellyfin. An owner that exists but is spelled in
another case keeps its item, and an item whose owner is empty or the all-zero id is kept
without an owner. A database the earlier version refused now opens. The pre-0044
snapshot and the adoption copy cover this boot too.

## Unreleased — playlists get their own folder and a new id, once

A playlist created in Ferrofin had no folder. Jellyfin gives each playlist one,
`<data>/playlists/<name>` (`<name>1` when that is taken), and derives the playlist's id
from it. A playlist made in Ferrofin is now stored the same way, so Jellyfin finds it in
its folder after a switch back.

On the first start after upgrading, each playlist an earlier version created moves to
its folder and its new id. Its entries, owner, shares, played state and favourite move
with it. The database file is copied to `<database>.pre-playlist-folders` first.

- **A playlist's id changes once.** A client or script that kept the old id has to look
  the playlist up again. Reloading the playlist list is enough in jellyfin-web and the apps.
- **Deleting a playlist now deletes its folder too**, as in Jellyfin.
- **An image uploaded to one of these playlists before the upgrade stays where it was.**
  A "Replace all metadata" refresh can still replace it with the generated collage. Upload
  the image again after upgrading to keep it.

## Unreleased — an API key must name the user a playlist is for

An API key carries no user. Earlier versions let `POST /Playlists` with an API key and
no `userId` create a playlist with no owner, which every account could then rename, edit,
share or delete. Ferrofin now answers `400 Bad Request`, as Jellyfin does: pass the id of
the account the playlist is for as `UserId` in the body (or `userId=`). `GET`/`POST
/Playlists/{id}/Items` take it as `userId=`. The other playlist routes (`GET`/`POST
/Playlists/{id}`, `Move` and the `Users` routes) act as the account whose token calls
them, so an API key always gets `400` from them, as in Jellyfin; call them with that
account's own token. Removing entries (`DELETE /Playlists/{id}/Items`) still works with an
API key, as in Jellyfin.

Playlists an API key already created stay ownerless, so every account keeps owner rights
over them, and only an administrator can delete one. To give one an owner, create it again
for that account, then delete the old one.

## Unreleased — a genre image drawn from music art stays until you delete it

Through 1.3.x, a genre whose movies and series had no poster could take music art
(album covers, song or music-video images) carrying the same genre name for its
generated image, which some clients show as the genre's tile. Jellyfin never draws that
image, and Ferrofin no longer does either, but an image that was already drawn stays.
A scan only redraws a generated genre image when its file changes on disk. Ferrofin also
has no record of which items an image was drawn from, so it cannot remove these
automatically.

To clear one, as an administrator, send `GET /Genres?searchTerm={name}`, take the `Id`
of the entry whose `Name` matches exactly, and send `DELETE /Items/{id}/Images/Primary`.
(`GET /Genres/{name}` cannot find a name containing `-`, such as Hip-Hop, because it
reads the dash as a URL slug.) The next full library scan (**Scan All Libraries** under
Dashboard → Libraries, or the scheduled scan) draws a new image from the genre's movie
and series posters if any exist. Otherwise it leaves the genre without one, as Jellyfin
does. Scanning a single library does not redraw genre images.

## Unreleased — the "Clean up collections and playlists" task is gone

Jellyfin 12 deleted this task, and Ferrofin has now removed it too. It ran about 3
seconds after every start and removed each collection and playlist member whose file it
could not see at that moment. If the media storage was not mounted yet (a late NFS/SMB
mount, a NAS slower to boot than the server, an empty bind mount), it emptied every
playlist and collection, and they stayed empty after the storage came back. Upgrading
does not bring back members it already removed; restore a backup taken before the loss
for that.

Nothing replaces it. Deleting an item already removes it from every playlist and
collection it was in. A member whose file you deleted stays listed until the next library
scan removes the item. Jellyfin 12 behaves the same way. Dashboard → Scheduled Tasks no
longer lists the task, and a trigger you saved for it is ignored.

## Unreleased — Ferrofin reports Jellyfin 12.2.0; Emby-era sign-in is off by default

`/System/Info` now reports `Version` `12.2.0` (1.3.x reported `10.11.8`). As in Jellyfin 12,
only the `Authorization: MediaBrowser …` header and the `ApiKey` query parameter
authenticate by default, on HTTP requests and the `/socket` WebSocket alike. The
Emby-era forms are refused while `EnableLegacyAuthorization` is `false`, the default on
a fresh install since 1.2.0:

- the `X-Emby-Authorization` header (sign-in returns `400`, "app is required")
- the bare `X-Emby-Token` and `X-MediaBrowser-Token` headers (`401`; `403` on the socket)
- the `api_key` query parameter, and the `Emby` scheme name in place of `MediaBrowser`

1.2.0 through 1.3.x refused them too on a fresh install, while reporting 10.11.8, a
version that accepts them by default. An install first set up on 1.0.x or 1.1.0 wrote
that release's default, `true`, into `config/system.json` and still accepts them. A
configuration adopted from Jellyfin keeps the value its `system.xml` had: on for a 10.11
server (Jellyfin 12 turns it off when it upgrades one; Ferrofin does not). Set it to
`false` in either case to match Jellyfin 12.

If a client that sends these forms cannot be updated, turn the flag on (Jellyfin
deprecated them, so only for such a client). jellyfin-web has no switch for it; use one of:

- **Running server:** as an administrator, `GET /System/Configuration`, set
  `"EnableLegacyAuthorization": true` and `POST` the whole document back to
  `/System/Configuration`. It takes effect on the next request, without a restart.
- **Stopped server:** set `"EnableLegacyAuthorization": true` in `config/system.json`.

The `/socket` WebSocket used to read its token itself: it took `api_key`, `X-Emby-Token`
and `X-MediaBrowser-Token` whatever the flag said, ignored the `Authorization` header, and
opened without any credentials (keep-alive only). It now authenticates like every other
request and, as in Jellyfin 12.2, refuses a connection without valid credentials with
`403` before the upgrade. The bundled jellyfin-web 12.x is unaffected (it sends
`?ApiKey=`), but a client whose socket sends `api_key` or a bare token header (jellyfin-web
10.11, for one) now loses its live updates and remote control unless the flag is on.
Clients built on jellyfin-sdk-kotlin send only the `Authorization` header on the socket;
their socket now joins their session, so remote control and SyncPlay reach them.

## Unreleased — a set-up server no longer starts without its database

This applies if setup has been completed (`IsStartupWizardCompleted` is `true` in
`config/system.json`, or in Jellyfin's `config/system.xml` for a configuration directory
not yet adopted) and the database Ferrofin would open is missing, is an empty file, or
records no migrations.

Until now such a server started anyway: it created an empty database and seeded a new
`admin` — with no password unless `FERROFIN_ADMIN_PASSWORD` is set — and every user, library
and watch history was gone. A deployment whose configuration directory was persisted but
whose database was not (a volume that did not mount, a wrong data directory) fell into
this quietly. Ferrofin now refuses to start, as Jellyfin 12.2 does, and exits with:

```
This server has been set up before (IsStartupWizardCompleted is true in <config>/system.json),
but the database does not exist (<data>/ferrofin.db). Ferrofin will not start an existing
server with an empty database. …
```

Pick one of the three ways out it names:

- **Restore the database** to the path it names — the usual fix when a mount or the data
  directory is wrong. A backup scheduled with `POST /Backup/Restore` still applies on the
  next start.
- **Start over, keeping this server's settings:** set `IsStartupWizardCompleted` to
  `false` in every file the message names (both `system.json` and `system.xml` on an
  adopted install). The next start runs the setup wizard on an empty database; users,
  watch history and everything else stored in the database do not come back.
- **Set up a new server** with empty configuration and data directories (`--data-dir` /
  `FERROFIN_DATA_DIR` / `data_dir`, and `FERROFIN_CONFIG_DIR` / `config_dir`).

A fresh install, a first boot in a new container and the adoption of a Jellyfin
directory that has its database are unaffected.

## Unreleased — Intro Skipper analyses your library again, once

This applies if Intro Skipper is enabled (Dashboard → Plugins), or if you are adopting a
Jellyfin database whose Intro Skipper plugin stored segments.

Intro Skipper now keeps its own record of every segment it found or you edited, and
publishes it to the skip buttons. On the upgrade, the segments already published are
copied into that record as automatic results with no settings fingerprint, as the
upstream plugin's own schema upgrade does. What follows:

- **The first analysis pass after the upgrade redoes the whole library**, and new
  fingerprints are taken (the fingerprint command now matches upstream's, and old prints
  are not reused). On a large library that is hours of ffmpeg time. It starts by itself
  at the task's next scheduled run (daily at 00:00 unless you changed it); run **Detect and Analyze
  Media Segments** from Dashboard → Scheduled Tasks to choose the time yourself.
- **Each copied segment is replaced by what that pass detects, or removed if it detects
  nothing.** Removed means the skip button for it disappears. With the same settings,
  detection finds the same intros again, so most libraries see no change.
- **Timestamps you corrected by hand are not kept.** Corrections made in Ferrofin before
  this upgrade were never recorded as yours, so they are treated as automatic. After the
  upgrade, a correction is recorded as yours and no later analysis replaces it.
- **Adopting a Jellyfin database:** the plugin's own database (which records your hand
  edits) and its settings file (`plugins/configurations/IntroSkipper.xml`) are not
  imported. Ferrofin's Intro Skipper starts on default settings, so set them again on
  its settings page before the first pass, or segments the plugin found under your old
  settings may not be found again. Your hand edits do not carry over.

## Unreleased — library scans in this release

Library scans keep the rule 1.3.0 introduced: an item is processed again only when
something about it changed, unless you choose **Search for missing metadata** or **Replace
all metadata** ([what a library scan does](FEATURES.md#what-a-library-scan-does)). This
release changes what a refresh does once it runs:

- Every checked metadata downloader runs, in the library's order, instead of only the
  first one that answers:
  [every checked metadata provider runs, in order](#unreleased--every-checked-metadata-provider-runs-in-order).
- Seasons get remote metadata, with TheTVDB first where the library saved no season order:
  [seasons ask TheTVDB before TheMovieDb by default](#unreleased--seasons-ask-thetvdb-before-themoviedb-by-default),
  and **Seasons get remote metadata** in the entry after it. An episode's TheTVDB id and
  a series' TheTVDB season list are reused for `FERROFIN_TVDB_CACHE_HOURS` hours (a new
  setting, default 1; see [`CONFIG.md`](CONFIG.md)).
- WASM plugins that supply metadata run at their place in the library's order, and an
  `album.nfo` or `artist.nfo` now wins over every remote provider:
  [plugin metadata sources run at their rank](#unreleased--plugin-metadata-sources-run-at-their-rank).
- In a library with **Extract trickplay images during the library scan** checked, a scan
  generates trickplay images several at a time, up to the image-encoding limit (one per
  CPU unless **Parallel image encoding limit** is set). Jellyfin generates one at a time.
  Each is an ffmpeg process with its own memory and, with hardware acceleration, its own
  GPU memory (about 1 GiB for a 4K HEVC video on an NVIDIA GPU). `FERROFIN_TRICKPLAY_CONCURRENCY` caps it (a new setting, default `auto`; `1`
  matches Jellyfin; see [`CONFIG.md`](CONFIG.md)).
- Sonarr's and Radarr's notifications now start a scan. They send camelCase keys
  (`{"updates":[{"path":…}]}`), which 1.3.x answered with `204` and ignored; request bodies
  now bind in any letter case, as Jellyfin's do. The reported path is scanned once
  `LibraryMonitorDelay` has passed, so a library on a network share that the disk watcher
  cannot see no longer waits for the next scheduled scan.
- A series identified by name on TheTVDB is searched and chosen as the TVDB plugin does:
  the search is the name without a trailing year or release tags (a `Show.2026` folder
  searches "show", not "show 2026"); results whose title (in the metadata language, or an
  alias) equals or contains the name rank first; and a known year drops every result more
  than a year away. 1.3.x searched with the year left in the name and took TheTVDB's first
  result unless one had exactly the item's year, so a folder whose name ends in a year
  could match an unrelated show.
  An already-matched series keeps its match: the ids that match stored (TheTVDB's, and the
  IMDb and TheMovieDb ids that came with it) win over a new search. To re-match one, use
  **Identify**, which replaces them; or clear its TheTVDB, IMDb and TheMovieDb ids in the
  metadata editor, then choose **Refresh metadata → Replace all metadata**.
- A season's and an episode's series name, and an episode's season name, are now those of
  its series and season, as in Jellyfin. 1.3.x kept the series name the folder gave
  ("Show 2026") on every season and episode, even after a provider named the series. The
  first start after upgrading corrects the rows saved before, once, and logs how many (on
  one test library, 3,223 seasons and episodes across 53 series); each changed row gets a
  new `Etag`, so clients refetch it once.

Apart from scans, deleting an item now deletes its files, and who may delete one follows
Jellyfin's rules:
[deleting an item now deletes its files](#unreleased--deleting-an-item-now-deletes-its-files-as-in-jellyfin).

**Upgrading from 1.3.x** causes no full pass: an unchanged item is still not reprocessed,
and each change above reaches an item at its next refresh that asks its providers. The
first scan does save every item once, because items are now stored as Jellyfin stores
them:
[the first scan after upgrading stores items as Jellyfin does](#unreleased--the-first-scan-after-upgrading-stores-items-as-jellyfin-does).

**Upgrading from 1.2.x or earlier:** also read the 1.3.0 entries below, starting with
[library scans process only what changed](#130--library-scans-process-only-what-changed):
the first scan after upgrading is one full pass, and every item's `Etag` changes once. That
entry also lists the settings that decide what a scan does, by their names in the
dashboard, including **Date added behavior for new content** and
`FERROFIN_METRICS_SCAN_DURATION_BUCKETS`.

Still open in this release:

- The first scan of an adopted Jellyfin database still rewrites two kinds of item: music
  albums, whose cover Ferrofin copies into its own folder where Jellyfin points at the
  album's `folder.jpg`; and music artists, whose running time Ferrofin sums from their
  tracks where Jellyfin stored 0.
- An episode that is not in a season folder is now filed under its series, as in
  Jellyfin; the season made for it (a season with no folder of its own) links it as its
  season, still lists it and still counts it, and clients still see that season as the
  episode's parent. 1.3.x filed such an episode under that season, which made the first
  scan of an adopted Jellyfin database save every one of them once (about 8,700 episodes
  under 591 seasons on a Jellyfin 12.1 test library); it no longer does. On a database
  1.3.x scanned, the first scan after upgrading saves each such episode once instead.
- Seasons with no folder of their own now take the ids Jellyfin gives them (from the
  series, the season number and the season's name). On a database an earlier Ferrofin
  scanned, the first start after upgrading moves each such season to its new id, with
  its watch state, images and episodes, after copying the database to
  `jellyfin.db.pre-season-ids`. As in Jellyfin, a new season is named in the server's
  display language ("Season 1", "Staffel 1", …, and "Season Unknown" for episodes with
  no season number), and season 0 takes the library's season 0 display name
  (`SeasonZeroDisplayName`). A season with no folder keeps the name and id it was
  created with when either setting changes later.

## Unreleased — the first scan after upgrading stores items as Jellyfin does

Ferrofin now stores library items the way Jellyfin stores them. What the first library scan
after upgrading does about it depends on where your database came from. Nothing on disk
has to change, and no watch history is lost.

**A database Ferrofin created itself** (no `jellyfin.db.pre-ferrofin` next to it) is
rewritten once by its next scan: every item is saved, and each item's `Etag` changes once.
That scan probes no file and asks no metadata provider, except for an item whose kind
changes (below), which is refreshed once as its new kind. It:

- files every item under a folder item for its library location, as Jellyfin does (each
  item's `TopParentId` and ancestors, and the parent of the items at the top of a
  location). Until that scan has run, a library is read through both, so nothing
  disappears in the meantime;
- clears the movie flag (`IsMovie`) on every movie: Jellyfin sets it only on Live TV
  programs and channels;
- stores no modification time (`DateModified`) and the media type `Unknown` for every
  folder-like item: series, seasons, albums, artists and folders;
- stores the shared-folder flag (`IsInMixedFolder`) of episodes, tracks and books as
  Jellyfin does;
- stores a DVD or Blu-ray rip as a video, not a folder (`IsFolder` 1 → 0);
- groups each movie's versions (`Movie - 1080p.mkv` beside `Movie - 2160p.mkv`) and
  stacked parts (`Movie - cd1.mkv`, `Movie - cd2.mkv`) under one item, as Jellyfin lists
  them, and likewise the versions and parts of an episode in one folder, so the movie and
  episode counts drop by one per version and part. The rows older scans made for them
  become that item's versions and parts;
- moves an item whose kind changed to the id of its new kind: a clip in a home-videos
  library, stored as a movie before, is now a video; files in a library with no content
  type become the episodes or videos Jellyfin resolves; a stacked part becomes a video.
  The item keeps its title, overview and artwork, but drops its provider ids unless it is
  locked.

Watch history, resume positions, favourites, playlist and collection entries and edits move
with every item that is regrouped or changes kind. Jellyfin itself would delete such an
item and create a new one.

**DVD and Blu-ray rips and multi-disc sets** now resolve as one video each, as in Jellyfin:
a folder holding `VIDEO_TS`, `BDMV` or `VIDEO_TS.IFO` is one video, and a folder of disc
folders that stack (`Heat (1995) - Disc 1`, `Heat (1995) - Disc 2`) is one video whose
other discs are parts. The rows older scans made per file or per disc fold into that video
with their user data. As in Jellyfin, **a rip hides any other media in its folder**: apart
from extras named for it, nothing else there or in its subfolders is resolved, and the scan
removes those items, logging a warning that names the folder and the first of the files.
Move such files out of a rip's folder to keep them in the library.

**Items Jellyfin never lists are removed from the library** by that scan; their files stay
on disk. Besides what a rip hides, that is a title inside a folder named like an extras
folder below the top of a library (`Collection/Other/Film (2001)/`), an album in such a
folder (one named `Other` inside an artist's folder), an album an older scan made of
each disc folder of a multi-disc album (now one album), and the album an older scan made of
a music library's loose top-level tracks (they are now tracks of their own). The watch
history of a removed item is set aside, as for any item whose file disappears, and returns
if the item is listed again.

**A folder no longer refreshes because its modification time changed**, as in Jellyfin:
adding an episode refreshes the new episode, not its season.

**A database adopted from Jellyfin 10.11** has its local versions and stacked parts
reshaped once at boot, before any scan, into the shape Jellyfin 12 gives them (Jellyfin's
own 12.0 upgrade deletes those rows and lets the next scan create them again; Ferrofin
keeps them with their user data). When there is work to do, the database file is first
copied to `jellyfin.db.pre-rehome`, next to the database, and the copy is never touched
again. Then the first scan rewrites only what Ferrofin still stores differently: on the
benchmark library (a database Jellyfin 10.11.8 scanned, 20,497 items, remote metadata
downloaders off), it updated 800 music albums (their cover) and 296 artists (their running
time), two of the differences listed as still open above, probed no file, and the second
scan wrote nothing. (The benchmark library keeps every episode in a season folder.)

**A database whose episodes were grouped with Group versions**, such as one adopted from
Jellyfin 12.1, has the copies of an episode that sit in one folder regrouped as that
episode's local versions on its first scan, as Jellyfin's own current scanner groups them.
On the Jellyfin 12.1 test database that rewrote about 828 episode rows.

Adoption stays one-way: going back to Jellyfin means restoring the
`jellyfin.db.pre-ferrofin` copy taken at adoption, which predates every change Ferrofin
made. `jellyfin.db.pre-rehome` is not a way back either; it is the database as it was just
before the reshaping above.

## Unreleased — deleting an item now deletes its files, as in Jellyfin

This applies to everyone who deletes media from Ferrofin: the **Delete media**,
**Delete Series**, **Delete Episode** or **Delete** entry in an item's menu, and
`DELETE /Items/{itemId}` / `DELETE /Items?ids=`.

**Deleting an item now deletes its files**, exactly as Jellyfin does. Before, Ferrofin
removed only the item from its database: the files stayed on disk and the next scan
brought the item back. Now:

- A movie (or other video) in its own folder takes **the whole folder** and everything in
  it: its extras, its other versions, its artwork and subtitles.
- A movie that shares a folder with others takes its file and every `.nfo`, image,
  subtitle or lyric file in that folder whose name **starts with** the movie's file name.
  Deleting `Alien.mkv` therefore also deletes `Aliens.nfo` and `Aliens-poster.jpg`, as
  Jellyfin does.
- A trailer or other extra kept beside its film, in the film's own folder, is deleted the
  way Jellyfin deletes it: **with the film's whole folder.** Remove such an extra from disk
  yourself if you want to keep the film.
- The same rule reaches further than you might expect, exactly as in Jellyfin:
  - a trailer or other extra kept **directly in a series folder** takes **the whole
    series folder** with it, every season and episode included;
  - one kept **directly in a season folder** takes **that season's folder**;
  - a home video that is the only clip in a subfolder of a home-videos library takes
    **that subfolder**, photos and anything else in it included.

  Delete such items from disk yourself if you want to keep what is around them.
- Three more cases are kept as Jellyfin has them:
  - deleting **one version** of a film kept in the film's own folder deletes that whole
    folder, **the other versions included**;
  - a single video in a folder that also holds subfolders is not counted as sharing its
    folder, so deleting it takes **the folder with its subfolders**;
  - deleting a DVD or Blu-ray rip, or a multi-disc set, deletes its folder, **with any
    other media inside it** that the rip or set hid from the library.
- An item with other versions (another cut or resolution of the same film, kept beside
  it or merged into it with **Group versions**) is replaced by its first remaining
  version, as in Jellyfin: that version keeps its own watch history, the item's playlist
  and collection entries move to it, and the other versions now belong to it. A film in
  its own folder still takes the whole folder, the versions beside it included; the next
  scan then removes those versions from the library. Deleting one version moves its
  playlist and collection entries to the item it is a version of.
- A series, season or album takes its folder; an episode takes only its own file.
- A trickplay folder saved next to the media goes with the item.

Check what a folder holds before deleting from it, and keep a backup of anything you are
not sure about.

Two safeguards differ from Jellyfin:

- **Scan each library once after upgrading before deleting from it.** Until a full scan of
  a library by this version has finished, a delete of an item in it is refused (`409
  Conflict`, and jellyfin-web shows an error) and nothing is deleted: rows an earlier
  Ferrofin stored can name more than the item — 1.3.x, for example, stored a music
  library's own folder as an album, and deleting that album would have deleted the whole
  library. Run **Scan All Libraries** (Dashboard → Libraries) or scan the library, then
  delete again. The same applies to a database adopted from Jellyfin. Collections and
  playlists are not affected.
- **A library's own folder is never deleted**, nor any folder above one, nor the server's
  collections and playlists folders. An item whose path is one of them shows no delete
  entry, and deleting it by id answers `401`; Jellyfin would delete the folder. No delete
  removes such a folder either: a video at a library's root that Ferrofin does not count
  as sharing the folder is refused rather than taking the library with it.

**If your media is mounted read-only** (a read-only NFS or SMB mount, a container volume
mounted `:ro`), a delete now fails: the server answers with an error and nothing of your
media is deleted, the item stays in the library. Some things have already happened by
then, as in Jellyfin, and are not undone:

- the artwork and metadata Ferrofin keeps for the item in its own data folder are
  cleared. Art a metadata refresh downloads comes back on the item's next refresh; **an
  image you uploaded, or art for a library whose image fetchers are turned off, does not
  come back**;
- if the item had other versions, the first remaining one has already taken its place:
  it is its own item now, the item's playlist and collection entries have moved to it,
  and versions whose files were already gone have been removed from the library;
- if the item was itself a version, its playlist and collection entries have moved to
  the item it is a version of, which no longer lists it.

To let Ferrofin delete media, mount it read-write for the server's user; otherwise
untick **Allow media deletion from** for your accounts so the delete entry is not
offered.

**Who may delete** follows Jellyfin more closely. Ferrofin 1.3.0–1.3.2 already refused
accounts without deletion rights; three things are refined:

- An account allowed to delete in some libraries only is matched to an item's library
  through the library tree, as Jellyfin does, instead of by comparing folder paths.
- The server answers a delete exactly as the item's own `CanDelete` says, which is what
  jellyfin-web shows its delete entry by, so the entry appears only where the delete is
  allowed. A *list* of playlists reports every playlist deletable, as Jellyfin's does;
  deleting a playlist still needs its owner or an administrator. A playlist with no owner
  (made with an API key by an earlier version, or created before Ferrofin stored playlist
  owners) can now be deleted by an administrator only; in 1.3.x any signed-in account
  could delete one.
- Some items cannot be deleted at all, as in Jellyfin: an item with no file of its own (a
  season Ferrofin groups from a series folder without season folders, a streamed item), a
  missing (virtual) episode or season, and a video a DVR recording is still writing.

The settings are under **Dashboard → Users → (user) → Profile → Allow media deletion
from**: **All libraries** lets the account delete in every library, or tick individual
libraries. New accounts have neither; an administrator needs one of them too (the
administrator Ferrofin creates on first start has **All libraries**). Deleting a collection
needs **Allow this user to manage collections** or an administrator. An account that may
not delete an item gets `401 Unauthorized`, and nothing is deleted; an item in a library
the account cannot see answers `404 Not Found`, as in Jellyfin, whatever its deletion
rights.

A delete now finishes even if the app that asked for it is closed half way, and
`DELETE /Items?ids=` handles the ids one at a time, in order, as Jellyfin does: if it
reaches an id the account may not delete, it stops with `401`, and the items before that
one are already deleted.

**Trickplay saved next to media** is now looked for where Jellyfin puts it. For a video
whose file name has more than one dot (`Alien.Resurrection.mkv`), that is
`Alien.Resurrection.trickplay`; Ferrofin looked for `Alien.trickplay`, which belongs to
`Alien.mkv`. For a folder item it is inside the folder. Ferrofin itself only writes
trickplay to its data folder, so nothing it made is left behind; trickplay a Jellyfin
install saved next to such files is now found, moved by the trickplay move task, and
deleted with its item.

## Unreleased — plugin metadata sources run at their rank

This applies only if you run WASM plugins that supply metadata (a plugin listed under
**Metadata downloaders** in a library's settings, or one whose documentation says it
fills metadata).

A plugin's metadata now takes its place among the library's metadata downloaders like
any provider, as in Jellyfin: the providers run in the order the library's settings
list them, and the first one to supply a field wins it. Before, a plugin always ran
after TheMovieDb, OMDb, TheTVDB, MusicBrainz and TheAudioDB and could only fill fields
they left empty.

- **Check the Metadata downloaders order of every library that uses a plugin.** A
  plugin listed above TheMovieDb (or another built-in provider) now wins the fields
  both supply: its overview, rating or genres replace TheMovieDb's on the next refresh
  that asks the providers. **This is likely even if you never moved it:** the dashboard
  shows a downloader that is missing from a library's saved order at the top of the
  list, and saving the library's settings stores the order shown. So in a library
  whose settings were saved before you installed the plugin, enabling the plugin put it
  first. To keep the old behaviour, move the plugin below the built-in providers and
  save.
- Where nothing ranks a plugin (a library that never saved its settings for that kind
  of item, or a plugin that does not appear in the list at all, which is never ranked),
  it runs after the built-in providers that come first by default (TheMovieDb for
  movies, series and episodes; OMDb; MusicBrainz; TheAudioDB) and only fills what they
  left. **Seasons are the exception:** TheTVDB and TheMovieDb's season provider have no
  default position either, and a plugin comes before them, so an unranked plugin runs
  first for seasons and wins every season field it supplies. That includes a plugin
  that does not list seasons among the kinds it supports: in a library with no saved
  season settings it is still asked about seasons (the supported kinds are not enforced
  yet, an open item). In a library with saved season settings, a plugin runs for
  seasons only where it is ticked, at its saved position (the top, if the saved order
  predates the plugin); one without `provider-info` (not in the list) runs after
  TheTVDB and TheMovieDb.
- Two things differ from before even where a plugin only fills gaps: its studios and
  tags are added to the other providers', and in a library whose language is not
  English its overview and tagline replace the English ones OMDb or TheAudioDB supplied
  (a plugin reports no language, so it counts as answering in the library's language,
  as in Jellyfin).
- **In a library you create from the dashboard, a plugin starts unticked** under
  *Metadata downloaders* and *Image fetchers*, as in Jellyfin: it is listed (for
  seasons, first), but runs only once you tick it. Before, it started ticked.
- For albums and artists, a plugin is now asked together with MusicBrainz and
  TheAudioDB, in the library's order, instead of during the file walk (after which
  MusicBrainz and TheAudioDB replaced whatever they also supplied). Artists that exist
  only as names on tracks are now asked too.
- An `album.nfo` or `artist.nfo` now wins over every remote provider, as in Jellyfin:
  MusicBrainz, TheAudioDB and plugins only fill what the NFO leaves empty. Before,
  MusicBrainz and TheAudioDB replaced the NFO's values (its genres, overview, year) on
  every refresh that asked them, and "Replace all metadata" erased NFO values they did
  not supply. Albums and artists refreshed before this release keep the replaced values
  until their next refresh that replaces values. **Search for missing metadata** fills
  only empty fields, so it does not bring them back; **Replace all metadata** on the
  music library does, but it also replaces every unlocked value you edited, so lock
  those first or refresh only the affected albums and artists.
- A plugin that fails (its metadata lookup returns an error) counts as a failed
  provider, as before: the item is asked again on the next scan, and the failure erases
  nothing.
- What a plugin can reach does not change: no filesystem, network only to the hosts it
  declares, the same memory and time limits.

For plugin authors: nothing to rebuild. The `ferrofin:plugin@0.5.0` world is unchanged
and existing components load as they are. Return only values you are confident in: your
plugin may well sit above TheMovieDb in a library's order (see above), and then your
answer wins. Your lookup's `provider-ids` now include the ids that providers ranked
before yours found in the same refresh (TheMovieDb's ids, for example). See
`docs/EXTENSIONS.md`.

No manual step is needed beyond checking the order.

## Unreleased — seasons ask TheTVDB before TheMovieDb by default

This affects TV libraries with no saved season settings: libraries created through the
API without season options, or whose metadata settings were never saved. There, both
TheTVDB and TheMovieDb are asked about seasons, and TheTVDB now goes first, as in Jellyfin
(where TheTVDB is a plugin, and plugins come first when two providers have no order). So
TheTVDB's season overview and id win, and TheMovieDb fills what it leaves (the dates, the
year, the cast). This includes seasons without a folder of their own, which were filled
from TheMovieDb alone before. Each season changes on its next refresh that replaces
values (a file joining or leaving its folder, **Replace all metadata**, or its own
**Refresh metadata** dialog); an unchanged scan changes nothing.

The dashboard now lists TheTVDB before TheMovieDb for seasons, and a new library saves
that order. Libraries whose settings were saved from the dashboard keep their saved order.
To keep TheMovieDb first, move it above TheTVDB under the library's season downloaders
and save.

## Unreleased — every checked metadata provider runs, in order

Metadata refreshes now ask every metadata downloader a library has checked, one
after the other in the library's order, as Jellyfin does. Before, the first
provider that answered was the only one asked. With the defaults that means
TheTVDB (series and episodes) and OMDb are asked after TheMovieDb. Expect more
provider requests on first scans and full refreshes, and on the scans that
re-ask about titles still missing an overview, trailer or Rotten Tomatoes score.
OMDb answers are cached for a day. To ask fewer providers, uncheck them in the
library's metadata settings.

- A later provider only fills what an earlier one left empty, cast included:
  TheTVDB's characters fill a cast TheMovieDb left empty, for example.
- An NFO's genres are no longer combined with the providers' genres; the NFO's
  list stands on its own, as in Jellyfin.
- The scans that re-ask about incomplete titles only fill empty fields. They no
  longer overwrite stored values, so your edits (even unlocked ones) and the
  stored cast and provider ids stay as they are. This is the automatic
  re-asking only: **Search for missing metadata**, which you start, also fills
  only empty fields but, as in Jellyfin, replaces an item's unlocked cast
  wherever a provider credits someone.
- **Seasons get remote metadata.** TheMovieDb's and TheTVDB's season providers
  now fill a season, as in Jellyfin, in the library's order for seasons (with no
  saved order, TheTVDB first: in Jellyfin it is a plugin, and plugins come first):
  TheMovieDb its overview, premiere date and year, cast and ids; TheTVDB its
  overview (in the library's language) and its TheTVDB id only, no date or
  year. A new library checks only TheTVDB for seasons, so there a season gets
  an overview and an id. A season keeps its folder's name unless TheMovieDb's
  "Import season name" setting is on. TheMovieDb's season overviews are in
  English for now, whatever the library's language (TheTVDB's follow it);
  asking TheMovieDb in the library's language is open work.
  - When: a season asks its providers when it is first scanned, when a file
    joins or leaves its folder, on **Search for missing metadata** and
    **Replace all metadata**, and when the library's refresh interval passes —
    never on an unchanged scan. Seasons without a folder of their own get the
    same from their **Refresh metadata** dialog.
  - Requests: where the library also uses TheMovieDb for episodes or for
    season images, the season's answer comes from the season request those
    already make, so a first scan asks nothing more; where it uses
    TheMovieDb for seasons only, each season's first scan costs one request.
    A season refreshed on its own (a file joining or leaving its folder, or
    its refresh dialog) costs one TheMovieDb request. TheTVDB asks one
    request per season, plus the series' season list at most once per
    `FERROFIN_TVDB_CACHE_HOURS` (default one hour; none when the series was
    just refreshed).
  - A season scanned before this release has no overview until one of those
    refreshes runs. To fill them now, run **Search for missing metadata** on
    the TV library once. Know what it costs first: it re-asks every checked
    provider about every series, season and episode of the library and
    re-reads every episode file. It only fills empty fields, except the cast:
    as in Jellyfin, a provider that credits someone replaces an item's
    unlocked cast. Lock the cast (or the item) where you have edited it.

No manual step is needed beyond that optional refresh.

## 1.3.2 — extras no longer appear as library children

Run **Scan All Libraries** once after upgrading if extras or samples appeared as
ordinary library items (#32). The scan repairs valid extras' ownership and library
relationships while retaining their IDs, metadata, artwork, and watch history,
including locked items. Special features, local trailers, and theme media remain
available through their owning movie.

The scan also removes database rows for files excluded by Jellyfin's discovery
rules, including AppleDouble files, ignored directories (#31), and extras in
folders without an eligible movie owner. It leaves the files on disk. Supported
sample extras such as `Movie-sample.mkv` remain; `sample.mkv` and
`Movie.sample.mkv` are ignored. Only successfully scanned locations are cleaned.
No database reset or forced metadata replacement is needed.

## 1.3.0 — library scans process only what changed

From 1.3.0 a library scan decides, item by item, what changed since the item was last
saved, and does only that work, as Jellyfin does. The scheduled scan, **Scan All
Libraries** and **Scan for new and updated files** probe, re-read and re-ask only new and
changed items; **Search for missing metadata** and **Replace all metadata** still process
everything they cover. A change the disk watcher or a webhook reports refreshes the nearest
item already in the library; that item's other children are checked, not processed.
[What a library scan does](FEATURES.md#what-a-library-scan-does) has the details, and
[`verify/`](../verify/README.md) checks a running server.

**The first scan after upgrading from 1.2.x or earlier is one full pass.** Those versions
cleared, on every scan, the date an item was last refreshed, so this scan finds every item
never refreshed. It probes every media file again, asks the metadata downloaders about every
item, downloads artwork for items that have none, and saves every item. Expect it to take
about as long as a scan took before. In 1.3.0–1.3.2 a refresh asked the checked
downloaders in order and stopped at the first that answered. From this release it asks
every checked downloader
([every checked metadata provider runs, in order](#unreleased--every-checked-metadata-provider-runs-in-order)),
so with several checked it can make more provider requests than one of those earlier scans
did.

From the second scan on, an unchanged item costs a stat. On the 20,497-item benchmark
library (remote metadata downloaders off), a scan of the unchanged library went from 151 s
to about 2 s, with no ffprobe run and no item written. With downloaders checked, which is
the default, a scan also pays for the re-asking described below: a 14,098-item library on
network storage, with its downloaders checked, took 133–158 s per scheduled scan on 1.3.2.
Upgrading from 1.3.x does not repeat the full pass.

**Each item's `Etag` changes once.** Jellyfin derives an item's `Etag` from the date the
item was last saved (`DateLastSaved`). Ferrofin 1.2.x and earlier never stored that date
(and a scan cleared the one an adopted Jellyfin database had), so every item reported the
same `Etag`. From 1.3.0 the date is written whenever an item is saved: each `Etag` changes
on the first scan above, and afterwards whenever the item is saved again. That is when a
scan, a refresh or an edit changes it, and for every item a **Search for missing
metadata** or **Replace all metadata** covers, since both save every item they cover. A
client that caches items by `Etag` fetches each item once more, then only the items that
were saved.

**The first scan after adopting a Jellyfin database saves every item once.** Nothing on
disk has to change for it: Ferrofin stores some values differently from Jellyfin, among
them the library an item is filed under (`TopParentId`, see
[the adoption entry](#130--a-database-adopted-from-jellyfin-shows-its-libraries-and-removes-deleted-media)),
some parent links (`ParentId`), the format of premiere dates, the shared-folder flag
(`IsInMixedFolder`), the movie flag (`IsMovie`) and folders' modification times. That scan
probes no file and asks no metadata provider, with two exceptions: the re-asking described
next, and an item Jellyfin never finished refreshing (no `DateLastRefreshed`), which is
refreshed in full, probe and providers included. Otherwise it only rewrites rows, and each
item's `Etag` changes once. It is a one-time cost: on the 20,497-item benchmark library (a
database Jellyfin 10.11.8 scanned, remote metadata downloaders off), it took about 80 s and
wrote about 9 GB, and every later scan of the unchanged library took about 2 s. With
downloaders checked, both scans also pay for the re-asking described next. A Jellyfin
database adopted and scanned by 1.2.x or earlier gets the full pass above instead. From
this release these values are stored as Jellyfin stores them, and that first scan rewrites
far fewer items:
[the first scan after upgrading stores items as Jellyfin does](#unreleased--the-first-scan-after-upgrading-stores-items-as-jellyfin-does).

**Titles still missing metadata are asked about on every scan.** This is a Ferrofin rule
that Jellyfin does not have, and the one kind of provider request a scan of an unchanged
library still makes. A movie or series with no overview, no trailer or (with OMDb checked)
no Rotten Tomatoes score, and an episode with no overview or only a placeholder title, is
asked about again on every scan. In 1.3.0–1.3.2 that re-ask stopped at the first downloader
that answered and merged by the Default rule, which replaces values: a provider value that
had drifted since the last save, such as a TheMovieDb rating, re-saved the item on every
scan. From this release
([every checked metadata provider runs, in order](#unreleased--every-checked-metadata-provider-runs-in-order))
every checked downloader is asked, the answer only fills empty fields, and the item is saved
only when it fills something, so a title the providers have no trailer or score for costs
its requests on every scan and nothing else.

The settings that decide what a scan does, as the dashboard names them:

- **Metadata downloaders** and **Image fetchers**, with their order, per kind of item
  (**Dashboard → Libraries → Manage library**): which providers a refresh asks. A
  library's own choices win; a kind it never saved choices for follows the server-wide
  options (see
  [that entry](#130--items-without-per-library-fetcher-choices-follow-the-server-wide-metadata-options)).
- **Automatically refresh metadata from the internet** (same page): when set, an item last
  refreshed longer ago than that is processed as if its file had changed. The default,
  **Never**, leaves unchanged items alone.
- **Enable real time monitoring** (same page): changes on disk are processed as they happen,
  once the folder has been quiet for `LibraryMonitorDelay` seconds. That is a server
  setting (default 60) with no field in the web client; set it through
  `POST /System/Configuration`. The `*arr` webhooks wait for the same delay.
- **Date added behavior for new content** (**Dashboard → Libraries → Display**): see
  [its entry](#130--date-added-behavior-for-new-content-is-honoured).
- `FERROFIN_SCAN_PROBE_CONCURRENCY`: how many ffprobe processes a scan runs at once.
- `FERROFIN_METRICS_SCAN_DURATION_BUCKETS`: the buckets of the scan-duration histograms,
  with metrics on (`FERROFIN_ENABLE_METRICS=true`). The scan metrics and the **Library
  scans** Grafana dashboard are described in
  [`contrib/metrics/README.md`](../contrib/metrics/README.md#library-scans-dashboard).

Both variables are in [`CONFIG.md`](CONFIG.md).

## 1.3.0 — subtitles download during library scans

Libraries with subtitle download languages now fetch missing subtitles when a movie
or episode is probed during a normal scan or full metadata refresh. Configure an
OpenSubtitles account in its plugin settings. The shared application key is built in.

Existing subtitles are retained, and unchanged scans make no subtitle requests.
Enabling languages for an already scanned library takes effect on its next video
probe or full refresh; the daily subtitle task also fills missing languages.
Provider outages leave the media scan and existing subtitles intact.

Both paths now respect provider disabling/order and perfect-match settings. Text
subtitles already embedded in a video satisfy their language, even when skipping
embedded image subtitles is disabled. The audio-language skip checks default audio
tracks, falling back to the first audio track when none is marked default.

## 1.3.0 — a database adopted from Jellyfin shows its libraries and removes deleted media

This applies only to a database adopted from a Jellyfin install (a
`jellyfin.db.pre-ferrofin` copy sits next to the database). A database Ferrofin created
itself is not affected.

Jellyfin files each library item under the folder of its library location, while
Ferrofin files the items it scans under the library itself. Earlier versions read an
adopted library through Jellyfin's folders only. After Ferrofin's first scan, each library
browsed empty (with empty counts, Latest and Next Up), and media deleted from disk was never
removed. Ferrofin now reads every library through both, and a scan (including one started
by the disk watcher or a `*arr` webhook) removes deleted media.

**The first library scan after this upgrade removes the entries for media deleted from
disk since Ferrofin first scanned the library (or since adoption, if it was never
scanned), with their watched state, resume positions and favourites.** Ferrofin does not
yet keep a deleted item's user data for when the file comes back, as Jellyfin does. So
before that scan, make sure your media is where the libraries expect it: a file that was
moved or renamed is removed, then added again as a new item without its watched state. A
scan never removes anything under a library location that is missing, empty or cannot be
listed (an unmounted drive or share), nor an entry whose file or folder is still on disk,
unless the scan replaced it with a new entry for the same file. That first scan also saves
every item once; see
[library scans process only what changed](#130--library-scans-process-only-what-changed).

Two things you may notice once an adopted library browses again, both gaps in how
Ferrofin's scan groups files (they are open work, not intended behaviour):

- **Alternate versions show as separate movies.** Jellyfin lists a movie with several
  versions (`Movie (2010) - 1080p.mkv` beside `Movie (2010) - 2160p.mkv`) once, with a
  version picker. Ferrofin 1.3.x does not group versions, so after its scan each file is a
  movie of its own, and the movie count rises by one per extra version. From this release
  versions are grouped as in Jellyfin
  ([the first scan after upgrading](#unreleased--the-first-scan-after-upgrading-stores-items-as-jellyfin-does)).
- **A plain subfolder in a movie library shows as an empty folder.** Jellyfin lists a
  folder that is not a movie's own (`Movies/Collection/Movie (2010)/…`) as a folder
  holding its movies. Ferrofin's scan lists those movies at the top of the library
  instead, and the folder Jellyfin created stays behind, empty, while its directory
  exists.

Adoption is still one-way: going back to Jellyfin means restoring the
`jellyfin.db.pre-ferrofin` copy taken at adoption, which predates every change Ferrofin
made.

## 1.3.0 — "Date added behavior for new content" is honoured

**Dashboard → Libraries → Display → Date added behavior for new content** now decides how
a new item's date added (`DateCreated`, what "Date Added" sorts and "Recently added"
read) is set. Earlier versions ignored it and always used the file's creation time.

- **Use file creation date** (the default, unchanged behaviour): a new file is dated by
  its creation time on disk. On Linux that is the file's birth time where the
  filesystem records one (ext4, btrfs, XFS, tmpfs), else the older of its change and
  modification times. A copy (`cp`, a download client's move across filesystems) is born
  when it is written, whatever its modification time says; a rename within a
  filesystem keeps its birth time. As in Jellyfin, a file whose modification time
  changes is re-dated to its creation time at the next scan.
- **Use date scanned into the library**: a new item is dated by the moment Ferrofin
  first detects it, whether a library scan or the disk watcher / a `*arr` webhook
  found it. Its series' date added moves with it at once.

Existing items keep their stored date: switching the setting re-dates nothing, and a
rescan never moves an unchanged item's date in either mode. The setting applies from the
next scan or watcher event, without a restart.

A date an item's own metadata supplies still wins, whichever the setting, and now also on
an item already in the library: an `.nfo` `<dateadded>` is applied whenever the `.nfo`
is read again (after it is edited), and a photo's EXIF date whenever the photo file
changes. Earlier versions applied either one only when the item was first added.

**If you adopted a Jellyfin install with an earlier Ferrofin, check this setting after
upgrading.** Adopting a Jellyfin install now imports its `metadata.xml`, and the import
also runs on the first boot of this version for an install adopted earlier, as long as its
configuration directory still holds Jellyfin's `metadata.xml` and the setting was never
saved in Ferrofin. If "Use date scanned into the library" was chosen in Jellyfin, this
version starts dating new items by when it detects them, and the dashboard shows that
choice, without any action from you. Items already in the library keep their dates. To go
back, choose **Use file creation date** under **Dashboard → Libraries → Display** and
save.

## 1.3.0 — items without per-library fetcher choices follow the server-wide metadata options

Scans and single-item refreshes (`POST /Items/{id}/Refresh`, Identify) now decide which
remote providers run for an item, and in what order, the way Jellyfin does. This applies to
every kind of item (movies, series, seasons, episodes, music videos, albums, artists, …):

- A library that saved its own choices for the item's kind keeps them: the **Metadata
  downloaders** and **Image fetchers** checkboxes and their order, under **Dashboard →
  Libraries → Manage library**.
- A kind the library never saved choices for (a library created by an earlier Ferrofin, or
  over the API without `TypeOptions`), and an item in no library at all (an artist known
  only by name, such as a compilation's album artist), now follow the **server-wide
  metadata options**: their disabled metadata and image fetchers and their fetcher orders.
  Earlier versions ignored those and ran every fetcher in the built-in order.

The server-wide options ship with Jellyfin's defaults, which turn off:

- **TheAudioDB** as a metadata downloader for music albums and music artists (its artwork
  stays on). Earlier versions asked it for every album and artist.
- **The Open Movie Database** as a metadata downloader and image fetcher for music videos
  (the library must enable it for those types).

If you customised the server-wide options — their disabled fetchers or their order, stored
in the server configuration's `MetadataOptions` — those settings now also apply to movies,
series and every other kind whose library saved no choices of its own. The web client has
no page for the server-wide options: they are edited through `POST /System/Configuration`
(`MetadataOptions`, one entry per item type), and stored in `system.json`.

The upgrade removes nothing already stored: descriptions and artwork a now-disabled
provider supplied stay, until a "Replace all metadata" refresh of the item clears what its
enabled providers do not return. To keep a provider for a library's items, open
**Dashboard → Libraries**, choose **Manage library**, tick the provider for each kind
(for TheAudioDB, **Music Albums** and **Music Artists**; for OMDb, **Music Videos**) and
save: the library then has saved choices, and its next scan uses them. Artists known only
by name have no library: remove `TheAudioDB` from the `MusicArtist` entry's
`DisabledMetadataFetchers` in the server-wide options to turn it back on for them.

## 1.3.0 — editing an item no longer locks it

Earlier versions locked an item (`LockData`) whenever a metadata-editor save changed one of
its fields, whether or not "Lock this item" was ticked. This version saves exactly what the
editor sends, and protects individual fields through the editor's per-field checkboxes
(`LockedFields`) instead, as Jellyfin does.

Items locked by an earlier version are not changed by the upgrade: there is no way to
tell an automatic lock from one you set on purpose. For those items:

- they stay locked, so no remote metadata provider (TMDB, TVDB, MusicBrainz, …) updates
  them;
- they do not read their `.nfo` while locked, which is how Jellyfin treats a locked
  item;
- their sidecar artwork next to the media (`poster.jpg`, `fanart.jpg`, …) is rediscovered
  on the next scan, as it would be for any locked item in Jellyfin;
- the metadata editor shows "Lock this item" ticked. Untick it and save to unlock the
  item; on a series, season, album, collection or other folder, that unlocks everything
  under it too.

To find them, list `GET /Items?Recursive=true&IsLocked=true`.

## 1.2.0 — the schema moves to Jellyfin 12.0

Back up the data directory before starting this version. On first boot migration `0032`
rebuilds `BaseItems`, `Users`, `Permissions`, `Preferences` and `MediaStreamInfos` into
Jellyfin 12.0's shape (the file is snapshotted to `jellyfin.db.pre-0032` first, next to
the existing `jellyfin.db.pre-0007`; 12.0 ships the same shapes as fourteen of Ferrofin's
own `BaseItems` indexes, so those are not recreated), `0033` rebuilds one index and drops `sqlite_stat1`,
and `0034` folds Ferrofin's playlist/collection cache table into Jellyfin's
`LinkedChildren`. Expect one longer start proportional to library size; a second boot is
a no-op. Every file-backed boot now runs `PRAGMA foreign_key_check` (about 0.14 s on a
42k-item library) and refuses to open a database that fails it.

Behaviour that changed with the shape:

- A playlist may now hold the same item more than once (Jellyfin 12 semantics); removing
  an entry removes every occurrence of that item.
- Two users whose names differ only by case can no longer coexist (`NormalizedUsername`
  is unique). Creating or renaming into a case-variant returns the error Jellyfin returns.
- `CleanName`/`CleanValue` use Jellyfin 12's punctuation-stripping form; a forced sort
  name goes through the full sort-name pipeline. Both are recomputed once on first boot.
- Localized user views (e.g. a Live TV view created under a translated name) are
  consolidated onto their name-independent id once, with channels, ancestors and
  display preferences moved along.
- **Adopting a Jellyfin database** now accepts 12.0.0, 12.1.0 and 12.2.0 as well as
  10.11.8–10.11.11 (exact migration sets, still one-way; a 12.x database baselines `0030`
  and `0032`, whose shape it already has). All six 10.11–12.1 releases passed live adoption
  tests on 2026-09-16; the 12.2 paths passed synthetic tests on 2026-10-13. See the
  [support matrix and tested build](../adoption/README.md#supported-and-tested-versions).
- A **Playlists** (or Collections) view is listed only when the user can see something in it,
  as in Jellyfin 12.2 — a playlist file inside a music album counts.
- **Playlist files in music libraries become playlists.** The next library scan turns each
  `.m3u`, `.m3u8`, `.pls`, `.wpl` and `.zpl` file in a music or untyped library, and each
  folder named `… [playlist]` or holding a `playlist.xml`, into a playlist that every user
  who can see the library can open, but nobody can edit; nothing inside such a folder stays
  an item. A `.pls` that an older scan listed as a song is removed as a song. The Playlists
  view now lists these, beside the ones made in the app.
- An **adopted** Jellyfin database can carry playlist files Jellyfin left empty (where it
  saved XML metadata beside them). Their entries are read when the file next changes; to read
  them now, refresh the library's metadata with "Search for missing metadata".
- Alternate-version groups are re-derived once from `LinkedChildren` (Jellyfin 12.1's
  `RepairAlternateVersionLinks`), and a version is hidden only while its primary exists in the
  same library: an item marked as a version of a row that no longer exists reappears in
  listings (144 episodes on the reference library), exactly as on Jellyfin 12.1. A 12.0 database keeps its `LinkedChildren` rows and
  is never re-imported from the frozen JSON copy in `Data`.

## 1.1.0 — Unicode username matching

Migration `0030_normalized_usernames.sql` owns the `Users.NormalizedUsername` column
and unique index. Older databases run it; adopted Jellyfin 10.11.10/10.11.11 databases
baseline it because they already have those schema objects. Migrations 0001–0029 remain
unchanged. A Rust data-only backfill then writes ICU-based invariant uppercase keys,
recording completion as `normalized_usernames_icu_v1` in `FerrofinMeta` in the same
transaction. It never adds a column or drops/recreates an index.

SQL initially copies the existing unique display names into the keys, so it does not
rely on SQLite's ASCII-only `upper()`. Startup checks for Unicode collisions before
running SQL migrations, and the backfill validates again inside its transaction.
No requests are served until the backfill succeeds. Account IDs, password hashes,
permissions, and watch history are preserved. A failed backfill can retry on the next
startup without reapplying SQL migrations.

Login, creation, and renaming now agree for non-ASCII case variants such as `münchen`
and `MÜNCHEN`. If old accounts normalize to the same key, startup refuses with their IDs
and names. Restore/use your pre-upgrade installation to rename the conflicting accounts,
then retry the upgrade; do not merge accounts. Back up the full data directory first.

For Jellyfin adoption, follow the [complete migration procedure](INSTALL.md#migrate-an-existing-jellyfin-installation),
including the separately stored configuration and copying before first startup.

## 1.0.0 — first public release

No manual steps between Ferrofin releases. The baseline for this file starts here;
pre-1.0 development builds were never published and are not an upgrade path.

**Coming from Jellyfin** is a different matter and is covered in the README under
[Migrating from Jellyfin](../README.md#migrating-from-jellyfin): adoption is one-way,
Ferrofin writes `jellyfin.db.pre-ferrofin` before touching anything, and you should back
up the whole Jellyfin data directory yourself first.

## Unicode metadata casing

Migration `0031_invariant_clean_names` corrects clean-name and derived sort keys
that match Ferrofin's previous full-lowercase mapping. It preserves item IDs,
references, raw names, custom keys, and the original forced sort-title text.
New Jellyfin-mode by-name IDs use .NET-compatible invariant casing; existing
person IDs and year IDs under affected Unicode metadata roots remain in use.

Apply this migration by starting Ferrofin. It uses application-provided Unicode
functions that standalone `sqlite3` and `sqlx migrate` do not register. No
persistent schema objects depend on these functions, so external database
inspection remains possible after migration. Follow the backup and rollback
steps above before upgrading.

## Shared provider keys

OMDb now uses Jellyfin's built-in API key when `FERROFIN_OMDB_KEY` / `omdb_api_key`
is unset or blank. Existing explicit keys still take precedence. Libraries that
have OMDb enabled can now fetch its metadata and artwork without extra setup;
to disable it, uncheck its metadata and image fetchers in the library settings.
This can add provider requests on installations where the checked provider was
previously inactive because no key was configured.

OpenSubtitles also defaults to Jellyfin's shared application key. Configure your
OpenSubtitles username and password in its plugin settings; `ApiKey` is optional.
The shared key does not replace the account required to download subtitles.
