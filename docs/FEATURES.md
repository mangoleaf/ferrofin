# Feature status

Ferrofin implements the Jellyfin server API surface, not a subset of it. Every one of the
**412 operations** in the vendored OpenAPI contract is wired to a real handler — there are
**no `501` stubs**. What varies is verification depth and, for a handful of operations, how
faithfully an edge case matches Jellyfin.

These tiers were last derived from the v2 parity ledger, retired with the v2 suite on
2026-09-02 (recoverable from git history). Verification tracking now lives in the source:
`handlers::VERIFIED` beside `REAL_ROUTES`, validated and printed by
`crates/ferrofin-api/tests/contract_superset.rs`, where **"deep verified" means the
Ferrofin implementation was compared against the upstream Jellyfin C# for behavioral
equivalence** — the Jellyfin release each row was compared against is `UPSTREAM_TAG` /
`UPSTREAM_COMMIT` in `crates/ferrofin-api/src/handlers/mod.rs`. That record is early:
**7 of 412 operations** carry a row today, so the tiers below remain the honest picture and
the `VERIFIED` count is the one that will replace them as it fills in.

Separately, [`bench/`](../bench/README.md) compares response *shapes* against Jellyfin
12.0-rc7 each run, on a generated library scanned by a real Jellyfin. That is supporting
evidence, not the parity number — it covers 28 requests and only the fields those requests
happen to exercise.

Headline as of the last v2 run — note this table uses the **retired v2** definition of
"deep-verified" (response + read-back diffed clean at runtime), which is a different and
weaker bar than the C#-comparison definition above, and is why its count is so much larger
than the 7 rows `VERIFIED` holds:

| | ops | |
|---|---:|---|
| Wired to a real handler (`REAL`) | **412 / 412** | 100% |
| `501` / hollow stubs | **0** | none |
| Deep-verified vs Jellyfin 10.11.8 | **241** | response + read-back diffed clean |
| Classified divergence | **146** | intentional, Jellyfin-bug-avoiding, or open work |
| Untested | **25** | awaiting a parity leg on the current tree |

Deep-verified means the response body **and** the persisted read-back were diffed against a
real `jellyfin/jellyfin:10.11.8` server on identical inputs and matched (for binary/asset
routes, the bytes were compared). Most of the rest are classified: a difference exists and
has been reviewed — usually because Ferrofin is correct where Jellyfin has a known bug
(see the "don't port Jellyfin bugs" policy), because the difference is a documented,
bounded simplification, or because it is named open work still to be ported.

The untested count is ops the harness had no measurement for. It is not a claim that
they are broken, and not a claim that they are fine — it is the ledger refusing to carry a
result forward from a tree that is no longer this one.

## Implemented & verified

Deep-verified against a real Jellyfin server:

- **Authentication & users** — `AuthenticateByName`, token auth, QuickConnect, API keys,
  password/policy management, user lockout, PBKDF2 hashes byte-compatible with Jellyfin.
- **Library** — scan/refresh, **live filesystem watching** (inotify) with debounced,
  path-scoped ingest; virtual folders; item read + write/edit + delete (a delete removes the
  item's files exactly as Jellyfin does — a video's own folder, prefix-matched sidecars —
  only for an account allowed to delete it: Dashboard → Users → Profile → "Allow media
  deletion from", All libraries or the item's library; on read-only media it fails and
  none of the media is deleted). Deep-verified for
  `movies` / `tvshows` / `music` / `homevideos` / `musicvideos` / `mixed` / untyped
  libraries; `books` is scanned too but is **not** deep-verified — see the entry below.
  `boxsets` is the one library type not resolved off disk (its members are curated through
  the collection API). A rescan reprocesses only what changed: see
  [What a library scan does](#what-a-library-scan-does) below, and
  [`verify/`](../verify/README.md) to check it against any running server. Movie and
  episode probes download missing subtitles in the library's configured languages,
  respecting existing subtitles and provider settings.
- **Browse & query** — the full `Items` query surface (filters, sorting, paging, fields),
  DTO shaping, genres/studios/persons/years, suggestions, InstantMix.
- **Images** — item/user/artist images, all image types, resize/crop/format, blurhash tags,
  the immutable-tagged-image caching contract.
- **Sessions & playback** — sessions, playstate reporting, remote control, capabilities, and
  the **WebSocket push** messages clients rely on (`UserDataChanged`, session updates, …).
- **Playlists & collections** — create/edit/reorder/share, membership, stored in Jellyfin's
  `Data`-JSON shape, the same source of truth Jellyfin itself reads.
- **Playback delivery** — direct play, and **live HLS transcode** including subtitle burn-in
  and fMP4 HEVC/AV1.
- **Hardware transcoding** — **NVENC, VAAPI and QSV**. Decode, scale, deinterlace, rotate,
  HDR→SDR tonemap and subtitle compositing all run on the GPU where the driver supports
  it, with the pipeline chosen from a runtime probe of the device rather than from
  configuration. NVENC is verified on real hardware; VAAPI and QSV are verified against
  the upstream argument shapes but **have not yet been run on an Intel or AMD GPU**.
  **AMF, VideoToolbox, RKMPP and V4L2M2M are not supported** — see the note in
  `CLAUDE.md`; selecting one falls back to a software transcode and logs a warning.
- **Live TV** — M3U **and HDHomeRun** tuners + XMLTV guide, channels/programs, DB-backed
  DVR timers & recordings. The HDHomeRun backend is a port of Jellyfin's `HdHomerunHost`:
  `discover.json`/`lineup.json`, UDP device discovery, the per-channel HTTP stream with
  its transcode profiles, and the legacy binary control protocol. **No physical HDHomeRun
  has been run against it** — it is verified against Jellyfin's own JSON fixtures, against
  a fake device on the parity lab's compose network that both servers consume (it answers
  the UDP discovery broadcast as well as the three HTTP documents, so `Detect my devices`
  is differentially diffed too), and (for the legacy control path) at the byte/CRC level
  plus a fake device that speaks the protocol back. Live TV as a whole is deep-verified at the API level only — **not yet
  exercised end-to-end with a real tuner/guide by a human**; treat as less battle-tested
  than the rest of this list.
- **SyncPlay** — groups, playback-command relay, time sync.
- **Scheduled tasks** — all 20 of Jellyfin's scheduled tasks (including the Live TV
  guide refresh and the hidden channel refresh) plus the trigger scheduler.
- **Observability** — Prometheus `/metrics` (Jellyfin-parity names), OTLP traces (opt-in).
- **Media detail** — trickplay, chapters, lyrics, media segments.
  - **Accepted divergence (a fix, not a gap):** Jellyfin's scaler for *half
    top-and-bottom* 3D sources carries an unbalanced bracket (`scale=(iw*2):ih)`), which
    ffmpeg's expression parser rejects outright — so chapter and thumbnail images cannot
    be extracted for that layout there at all. Ferrofin emits the balanced form, the same
    shape Jellyfin's own half-side-by-side case already has.
- **Photos & books** — a home-videos library resolves its images into `Photo` items with
  their EXIF read off the file (camera, exposure, GPS, orientation, date taken); a books
  library resolves `.epub`/`.cbz`/… into `Book` items with `ComicInfo`/`ComicBookInfo`/OPF
  metadata and the cover extracted from the archive.
- **Item links & id fields** — the "Links" row (IMDb/TMDB/MusicBrainz/…) and the per-kind
  external-id fields the Identify dialog offers.
- **Backup & restore.**

## Implemented, less battle-tested / known partial

Wired and working, with a documented limitation or lighter verification:

- **`LiveTv/Programs` filter params** — a few query params (3 ops) are accepted but not yet
  honored as filters.
- **Similar items** — the local weighted genre/tag/people scorer always runs; the remote
  providers (TMDB similar titles, ListenBrainz similar artists) run only for a library
  that ticked them under "Similarity providers", and resolve against items already in the
  library. The local scorer is a single query rather than upstream's six per-kind
  providers, which are identical in behaviour.
- **Remote metadata providers** (TMDB / OMDb / TVDB / MusicBrainz / AudioDb / fanart / Studio Images)
  — compiled in and **on by default** with built-in keys, gated per library by the
  "Metadata downloaders" / "Image fetchers" checkboxes. OMDb uses Jellyfin's shared
  key unless `FERROFIN_OMDB_KEY` (config `omdb_api_key`) supplies an override.
- **DLNA** — the profile / `StreamBuilder` logic is ported (used for transcode decisions), but
  there is no DLNA **server** side.
- **Books / audiobooks** — a `books` library resolves documents (`.azw .azw3 .cb7 .cbr .cbt
  .cbz .epub .mobi .pdf`) to `Book` and audio files to `AudioBook`, and serves them through
  `/Items/{id}/File` + `/Items/{id}/Download`, which is what jellyfin-web's epub/comic/pdf
  readers fetch. Verified against Ferrofin over real HTTP and in unit tests, but **not
  diffed against a live Jellyfin server** — treat it as the least-verified entry here.
  Notable behaviours and divergences:
  - **Accepted divergence (ahead of the contract):** name / series / index / year come from
    `Emby.Naming.Book.BookFileNameParser`, which is on upstream `master` and **not** in the
    pinned 10.11.8 contract. Against 10.11.8 a book is named from its bare filename; Ferrofin
    parses `A Study in Scarlet (Sherlock Holmes, #1) (1887)` into its parts.
  - **Faithful upstream limitation:** a multi-file audiobook is **one item per file**, not one
    stacked item. `AudioResolver` skips stacked results outright ("until multi-part books are
    handled"), and `ResolvePaths` then falls back to per-file resolution — Ferrofin reproduces
    that rather than inventing stacking Jellyfin clients have never seen.
  - **Flattening divergence:** upstream turns a folder it cannot resolve to a book into a
    `Folder` item and parents the books under it; Ferrofin parents every book directly to the
    collection folder, exactly as the movie scan does. This scanner materializes no
    intermediate `Folder` rows.
  - **Naming divergence at the library root:** a books library whose *root* holds exactly one
    audio file is named after the **library folder** by Jellyfin (and dated from it) — an
    artefact of the root going through the multi-item resolver. Ferrofin names it from the
    file, with no year. Naming a book after the library it sits in is an upstream wart, not
    behaviour worth reproducing; every other shape matches upstream exactly.
  - Metadata comes from the file itself: `ComicInfo.xml` (inside the archive or beside it),
    the ComicBookInfo JSON in a `.cbz`'s archive comment, and EPUB/OPF Dublin Core + Calibre
    fields, with the cover extracted from the archive. There is still no *remote* book
    provider — that is the third-party Bookshelf plugin.
  - **`.cbr` / `.cb7`** are recognized and browsable, but yield no embedded metadata or
    cover: those are RAR and 7z archives, and neither has a maintained pure-Rust reader
    worth the dependency. `.cbz` and `.cbt` are fully read.
- **Photo keywords** — the EXIF pass fills every field Jellyfin's does except `Genres` and
  `Tags`, which upstream aggregates from XMP/IPTC keywords.
- **`collection.xml` / `playlist.xml` / `.m3u` playlist files** — the readers and writers are
  ported and tested, but nothing calls them yet: Ferrofin creates collections and playlists as
  pathless database rows, and its scanner resolves no collection/playlist *folders*, so there
  is no on-disk file to read or write. Membership lives in `BaseItems."Data"` (Jellyfin's own
  source of truth), which is what makes in-place adoption of a Jellyfin database work.

## Not implemented (by design)

Precise about what's absent — this is what keeps the rest of the matrix credible:

- **.NET-style native plugin loading** — never (no stable Rust ABI; full-trust loading is
  rejected by design). In-process plugins ship as compiled-in extensions (Tier 1a) or
  sandboxed, runtime-installed WASM components (Tier 1b) — see
  [`EXTENSIONS.md`](EXTENSIONS.md). WASM plugins install from configured plugin
  repositories over the dashboard's catalog (download → verify → stage → restart,
  Jellyfin's flow); uninstalling a compiled-in plugin is still rejected.
- **DLNA server discovery (SSDP)** — no SSDP broadcast/discovery.

## What a library scan does

A scan decides, item by item, what changed since the item was last saved, and does only
that work. The decision is a port of Jellyfin's own (`MetadataService.RefreshMetadata`), so
the three choices of jellyfin-web's **Refresh metadata** dialog do what they do on
Jellyfin. Every scan still walks the library's folders, which is how new files are found and
deleted ones removed; the walk is the cheap part. On the 20,497-item benchmark library
(remote metadata downloaders off), a scan of an unchanged library takes about 2 s, with no
ffprobe run and no item written (151 s before 1.3.0, which re-probed and re-saved every item
on every scan). With downloaders checked, which is the default, a scan also pays for the
re-asking described below: a 14,098-item library on network storage, with its downloaders
checked, took 133–158 s per scheduled scan on 1.3.2.

### The three choices

The dialog opens from **Scan library** on a library's card under **Dashboard → Libraries**,
and from **Refresh metadata** in an item's or folder's menu, where it covers that item and
everything under it.

- **Scan for new and updated files** (the dialog's default):
  - A new file is added: probed with ffprobe, its NFO read, the library's checked
    **Metadata downloaders** asked in their order, and its artwork downloaded.
  - An item already in the library is processed again only when something about it
    changed:
    - its file's or folder's modification time moved by more than a second: it is
      processed in full, a media file probed again, its NFO read again and its downloaders
      asked again;
    - its NFO was saved more than a minute after the item was: the NFO is read again;
    - the subtitle or audio files beside a video changed, or the item has no media
      information yet: the file is probed again;
    - the library's **Automatically refresh metadata from the internet** interval is set
      and has passed since the item's last refresh: as for a changed file.

    What is read or fetched then replaces the item's unlocked values.
  - Every other item is only checked, a stat and a comparison: no probe, no provider
    request, no write. Artwork files next to the media are checked on every scan, so a new
    `poster.jpg` is found; artwork is downloaded only for new items and items never
    refreshed before.
  - One exception is Ferrofin's own rule, which Jellyfin does not have: a movie or series
    with no overview, no trailer or (with OMDb checked) no Rotten Tomatoes score, and an
    episode with no overview or only a placeholder title, has its downloaders asked again
    on every scan. The exact checks depend on which downloader comes first. Such an answer
    only fills empty fields, and the item is saved only when it fills something.
- **Search for missing metadata**: every media file is probed again, every NFO read again
  and every checked downloader asked, and every item is saved. Values only fill empty
  fields, so edits stay, unlocked ones included. The exception is the cast: as in Jellyfin,
  a provider that credits someone replaces an item's unlocked cast. Artwork is downloaded
  for items that have none; with **Replace existing images** ticked it is downloaded again
  over what they have, except artwork stored next to the media.
- **Replace all metadata**: as **Search for missing metadata**, but the NFO's and the
  providers' values replace the item's unlocked values, and an unlocked field none of them
  supplies is cleared. When a provider failed and none answered (none could be reached,
  say), nothing is cleared: the NFO's values still replace, and the rest stays.

Locks hold under all three choices. A field unticked under the metadata editor's **Enabled
Fields** keeps its value. An item with **Lock this item to prevent future metadata
changes** ticked reads no NFO and asks no metadata provider. Its file is still probed when
it changed and under the two full choices, and those two can still download its artwork
(when it has none, or with **Replace existing images** ticked).

**Scan All Libraries**, the **Scan Media Library** scheduled task (every 12 hours by
default) and `POST /Library/Refresh` run **Scan for new and updated files** over every
library.

### Changes on disk and `*arr` webhooks

With **Enable real time monitoring** ticked in a library's settings, Ferrofin watches the
library's folders and processes a change once they have been quiet for
`LibraryMonitorDelay` seconds. That is a server setting, 60 by default, with no field in
jellyfin-web: it is set through `POST /System/Configuration`. The `*arr` webhooks
(`POST /Library/Media/Updated`, `/Library/Series/Updated`, `/Library/Movies/Updated`) wait
for the same delay, with or without real time monitoring (but see the Sonarr limit below).
Either way, as Jellyfin's `FileRefresher` does, the scan starts at the nearest item already
in the library and runs **Scan for new and updated files** on it. For a new episode that is
its season: the episode is added, the season (whose folder changed) is refreshed, the
season's other episodes are checked but not probed or saved unless they changed too, and
nothing else in the library is walked. The series' **Date Episode Added** moves at once,
where Jellyfin waits for the series' own next refresh. Measured with every Series, Season
and Episode downloader off: a new episode appears about `LibraryMonitorDelay` seconds after
its file lands (5.1 s with a 5 s delay), and the scan itself takes about 50 ms. Provider
requests come on top: the new episode's first refresh asks every checked downloader, and
the season's folder change asks the season providers.

### Known limits

- **Sonarr's notification is not read yet.** Sonarr sends camelCase keys
  (`{"updates":[{"path":…}]}`), and Ferrofin binds only the PascalCase keys of Jellyfin's
  API schema (`Updates`, `Path`). It answers `204` and scans nothing; Jellyfin binds the
  keys in any letter case. Radarr shares Sonarr's code and is expected to send the same.
  Until this is fixed, a library whose changes the watcher cannot see (a network share
  written to by another host) is updated by scheduled scans only.
- **A file that disappears takes its item's user data with it.** A deleted, renamed or moved
  file's item is removed with its watched state, resume position and favourite, and a
  renamed or moved file comes back as a new item. Jellyfin keeps that data for when the
  file returns; porting that is open work. A scan never removes anything under a library
  location that is missing, empty or cannot be listed (an unmounted drive or share).
- **Artwork is downloaded only for an item that has none.** Jellyfin also downloads each
  missing image type of an item that has some, such as a backdrop beside an existing poster.
- **Replace existing trickplay images** does nothing during a scan yet: trickplay images
  come only from the **Generate Trickplay Images** scheduled task.

### After an upgrade or an adoption

- The first scan after upgrading from 1.2.x or earlier is one full pass: those versions
  kept no record of when an item was last refreshed. See
  [UPGRADING](UPGRADING.md#130--library-scans-process-only-what-changed).
- The first scan after adopting a Jellyfin database saves every item once, because
  Ferrofin stores some values differently from Jellyfin. It probes no file and asks no
  provider, with two exceptions: the re-asking above, and an item Jellyfin never finished
  refreshing (no `DateLastRefreshed`), which is refreshed in full, probe and providers
  included. Later scans of an unchanged library save nothing. See the same section.
- With metrics on (`FERROFIN_ENABLE_METRICS=true`), `ferrofin_library_scan_items_total`
  counts each scan's items by outcome (`created`, `updated`, `unchanged`, `removed`),
  `ferrofin_media_probe_total` its ffprobe runs and
  `ferrofin_metadata_provider_requests_total` its provider requests; the
  [Library scans dashboard](../contrib/metrics/README.md#library-scans-dashboard) plots them.

## Regenerating this

`docs/FEATURES.md` is written by hand. Per-operation verification tracking is
`handlers::VERIFIED` — add a row there when you have compared an operation against the
upstream C#, and `contract_superset.rs` will check it maps to a real contract operation
and print the count. When adding or changing an operation, revisit the tiers above.
