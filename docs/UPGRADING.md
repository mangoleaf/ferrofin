# Upgrading Ferrofin

Operator-facing notes for upgrades that need a manual step or change behaviour in a
way the release notes do not make obvious. Newest first. `CHANGELOG.md` lists *what*
changed; this file says *what you have to do about it*.

Ferrofin's own database upgrades in place: start the new version against the same
data directory and its migrations run on boot. Back up the data directory before a
major-version upgrade.

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

Apart from scans, who may delete an item now follows Jellyfin's rules:
[who may delete an item](#unreleased--who-may-delete-an-item-follows-jellyfin-more-closely).

**Upgrading from 1.3.x** causes no full pass: an unchanged item is still not reprocessed,
and each change above reaches an item at its next refresh that asks its providers.

**Upgrading from 1.2.x or earlier:** also read the 1.3.0 entries below, starting with
[library scans process only what changed](#130--library-scans-process-only-what-changed):
the first scan after upgrading is one full pass, and every item's `Etag` changes once. That
entry also lists the settings that decide what a scan does, by their names in the
dashboard, including **Date added behavior for new content** and
`FERROFIN_METRICS_SCAN_DURATION_BUCKETS`.

Still open in this release:

- Deleting an item leaves its media files on disk.
- Sonarr's webhook notification is answered but not read: Sonarr sends camelCase keys
  (`updates`, `path`), which Ferrofin does not bind yet, so nothing is scanned. Radarr
  shares Sonarr's code and is expected to behave the same. A library Sonarr writes to is
  updated by the disk watcher (where it can see the change) and by scheduled scans.
- The first scan of an adopted Jellyfin database saves every item once, as described in
  the 1.3.0 entry.

## Unreleased — who may delete an item follows Jellyfin more closely

This applies to accounts that delete items: the **Delete media**, **Delete Series**,
**Delete Episode** or **Delete** entry in an item's menu, and `DELETE /Items/{itemId}` /
`DELETE /Items?ids=`.

Ferrofin 1.3.0–1.3.2 already refused a delete from an account without deletion rights. Three
things are refined, to match Jellyfin:

- An account allowed to delete in some libraries only is matched to an item's library
  through the library tree, as Jellyfin does, instead of by comparing folder paths.
- The server answers a delete exactly as the item's own `CanDelete` says, which is what
  jellyfin-web shows its delete entry by, so the entry appears only where the delete is
  allowed. A *list* of playlists reports every playlist deletable, as Jellyfin's does;
  deleting a playlist still needs its owner or an administrator. A playlist with no owner
  (made with an API key, or created before Ferrofin stored playlist owners) can now be
  deleted by an administrator only; in 1.3.x any signed-in account could delete one.
- Some items cannot be deleted at all, as in Jellyfin: an item with no file of its own (a
  season Ferrofin groups from a series folder without season folders, a streamed item), a
  missing (virtual) episode or season, and a video a DVR recording is still writing.

The settings are under **Dashboard → Users → (user) → Profile → Allow media deletion
from**: **All libraries** lets the account delete in every library, or tick individual
libraries. New accounts have neither; an administrator needs one of them too (the
administrator Ferrofin creates on first start has **All libraries**). Deleting a collection
needs **Allow this user to manage collections** or an administrator. An account that may
not delete an item gets `401 Unauthorized`, and nothing is deleted.

**Deleting an item still removes it from the library database only: its media files stay
on disk**, and the next scan finds them again. Jellyfin also deletes the files; a later
release will too, the way Jellyfin does.

One more difference from Jellyfin remains: an account with **All libraries** deletion that can
see only some libraries can still delete, by id, an item in a library it cannot see, where
Jellyfin answers `404`. A later release closes this.

`DELETE /Items?ids=` handles the ids one at a time, in order, as Jellyfin does: if it
reaches an id the account may not delete, it stops with `401`, and the items before that
one are already deleted. A delete finishes even if the app that asked for it is closed
half way.

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
database adopted and scanned by 1.2.x or earlier gets the full pass above instead. Storing
these values as Jellyfin does, so that adoption needs no such pass, is open work.

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
  version picker. Ferrofin does not group versions yet, so after its scan each file is a
  movie of its own, and the movie count rises by one per extra version.
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
- **Adopting a Jellyfin database** now accepts 12.0.0 and 12.1.0 as well as 10.11.8–10.11.11
  (exact migration sets, still one-way; a 12.x database baselines `0030` and `0032`, whose
  shape it already has). All six releases passed live adoption tests on 2026-09-16,
  including both 10.11.8 → 12.1.0 and 10.11.8 → 12.0.0 → 12.1.0. See the
  [support matrix and tested build](../adoption/README.md#supported-and-tested-versions).
- A **Playlists** (or Collections) view is listed only when the user can see something in it,
  as in Jellyfin 12.1. A library whose playlists all live inside music album folders — every
  `.m3u` next to an album — has no Playlists view on the home screen; the playlists themselves
  are unchanged and still found by search and `/Items`.
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
