# Changelog

All notable changes to Ferrofin are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com) and
[Conventional Commits](https://www.conventionalcommits.org); versions follow
[Semantic Versioning](https://semver.org).

Upgrades needing a manual step or with a non-obvious behavior change are
called out in [docs/UPGRADING.md](docs/UPGRADING.md).

## [Unreleased]

### Database
- Adopt Jellyfin 12.3 and 12.4 databases in place: neither release adds a migration, so each
  records 12.2's migration-id set and adopts as generation `12.2.0`. The adoption suite gains
  their routes (from 10.11.8, from the previous 12.x release, and a fresh install of each), checks
  that each records its 12.2 counterpart's id set, and moves its smoke, seed and messy-shape
  oracles to Jellyfin 12.4
- Rebuild the Live TV channel table without its cascading key to the tuner hosts
  (migration 0046), keeping every channel and airing; the guide refresh removes a deleted
  tuner's channels
- Converge the schema on Jellyfin 12.0 (migrations 0032–0034): table rebuilds with a
  pre-rebuild snapshot, 12.0's index set (Ferrofin's 14 duplicate `BaseItems` indexes are
  not recreated), `LinkedChildren` as the only membership store
- Pin id-list queries to the primary key: 12.0 drops
  `IX_BaseItems_Id_Type_IsFolder_IsVirtualItem`, and without the `+"Type"` pin the
  planner scanned every row of the type (`detail:similar` 4.9 → 7.8 ms p50)
- Adopt Jellyfin 12.0, 12.1 and 12.2 databases in place alongside 10.11.8–10.11.11 (exact
  migration sets per generation, 12.1/12.2 re-dated routines optional; `0030` is baselined
  where the database already owns `NormalizedUsername`; an atomic adoption record drives the
  one-shot membership import)
- Run `PRAGMA foreign_key_check` on every boot, not only after a migration
- Snapshot the database before any pending migration, as Jellyfin does: a boot that migrates
  copies the file to `<db>.pre-NNNN` first and deletes the copy once its migrations and the
  foreign-key check have passed; a failed boot keeps it, and a copy that cannot be written
  only warns (Jellyfin stops its startup). The copies before `0007`, `0032` and `0044` are
  kept for good, as before
- Write every database snapshot (`.pre-ferrofin`, `.pre-NNNN`, the boot repairs' copies) to
  `<name>.partial` and rename it only once it is complete and on disk, so a full disk or a
  kill never leaves a cut-short copy under the real name; the first snapshot taken is kept
  on a retry instead of being replaced
- Take the copies made before migrating or adopting with `VACUUM INTO` when another process
  is reading or writing the database, instead of a file copy that missed what was still in
  the write-ahead log
- On a database adopted from Jellyfin 10.11, remove once the items whose file or library is
  gone, as Jellyfin 12's `MigrateLinkedChildren` does, with its guards for unmounted media
  folders; play state moves to the placeholder item

### Tooling
- A release's A/B gate compares against the latest release of the minor line before it (v1.4.1
  against v1.3.2): for a release (its `release/vX.Y.Z` branch, or `--release vX.Y.Z` for a patch cut
  from `main`), `gates/run --previous` and every `gates/bench/gate.py` mode that takes a previous
  image refuse any other one and name the right one
- `gates/adoption/run.sh` stops at once when terminated, under bash 5.2 too (a TERM was
  sometimes lost and the run waited out its deadlines); GitHub's shell-tests job gets a
  15-minute timeout
- `gates/parity/filed.toml` records the two differences of the 2026-10-24 replay that have an open
  issue and no ledger entry (the HLS video segment's timestamps, the live HLS segment names); the
  run's other groups are accepted by the ledger, gone under the current comparison, fixed since,
  or came from the replay's older harness
- The item repository's query translates eleven filter fields it used to ignore (trailer types,
  chapter images, the provider-id maps, dead parents, box-set library folders and the four "has
  no track in this language" filters), each with Jellyfin's alternate-version and folder roll-up
  where it has one; no route sets them yet. A test now fails when a query field is read nowhere
- The four live checks moved under `gates/`: `bench/` → `gates/bench/`, `adoption/` →
  `gates/adoption/`, `parity/` → `gates/parity/`, `verify/` → `gates/verify/`. Every gate
  takes its directories from one output root, `FERROFIN_GATES_DIR` (absolute, on a local
  disk, outside the repository): fixtures in `fixtures/` (`adoption-<key>`,
  `bench-testdata`), scratch in `work/<gate>/`, runs in `runs/<gate>-<name>/`
  (`gates/lib.sh`). `--fixtures`, `--testdata`, `--out` and `ADOPTION_WORK_DIR` still win;
  with neither, a gate stops and names the variable. An existing corpus or run directory
  (`bench/testdata/`, `bench/runs/`, `parity/runs/`) is moved under the root by hand (a run
  renamed `bench-<name>` or `parity-<name>`), or passed with `--testdata` or `--out`
- `gates/run <gate|all>` validates one commit with one image (`gates/README.md`). A preflight
  names every cause that would stop it before any gate starts: a missing tool, an image
  missing or not built from the checked-out commit (its `org.opencontainers.image.revision`
  label, set with `docker build --build-arg SOURCE_COMMIT=…`), a fixture not built, another
  session holding the root's lock, busy cores, or a container another run left. Each gate
  then writes into one session folder, `runs/<branch>-<YYYYMMDD-HHMM>/`, beside a
  `session.json` with every gate's status. `gates/verify/run.sh` runs the scan-behaviour
  check against a fresh server started from an image, and `gates/bench/run.sh` measures
  `FERROFIN_BENCH_IMAGE` (default `ferrofin:bench`)
- Every gates session ends with one report, `index.html` and `report.md` in its folder
  (`gates/report.py`): the commit, images and oracle pin, and each gate's verdict and counts
  (verify's checks, adoption's rows, parity's new and unfiled differences, bench's phases or
  gated A/B cells), with links to its own report. Adoption, parity and the A/B gate are judged
  by their own code. A gate that did not run, or whose result is missing or unreadable, is
  shown as such. `scan-behaviour.sh --json FILE` writes its rows as data
- A root `Makefile` for the gates: `make run-gates` runs all four, `make run-verify`,
  `run-adoption`, `run-parity` and `run-bench` one (`IMAGE=` and `PREVIOUS=` passed to
  `gates/run`), and `make test-gates-offline` the gates' own shellcheck, bats and Python tests,
  which CI's shell-tests job now runs through it
- `gates/adoption/`: an adoption smoke harness that runs every supported Jellyfin generation
  (10.11.8, 10.11.9, 10.11.10, 10.11.11, 12.0.0, 12.1.0, 12.2.0) through an image on fresh
  fixture copies and diffs the answers against Jellyfin 12.2's own; fixtures are derived
  from one supplied 10.11.8 snapshot
- All seven 10.11–12.1 live fixture paths passed on 2026-09-16, including both 12.1 upgrade
  routes; 12.2 paths passed the synthetic suite on 2026-10-13.
  See the [support matrix and tested build](gates/adoption/README.md#supported-and-tested-versions).
  The harness now reports SQLite query failures and detects version promotions when the
  repaired count is zero, with five regression tests; all 40 shell tests passed.
- Every dependency is declared once, in the root `[workspace.dependencies]`, and used: the
  unused `axum-extra`, `argon2`, `insta` and seven dead crate edges are gone, and the backup
  handler's inline zip 2 and ferrofin-util's rand 0.8 move to the workspace's zip and rand 0.10
- One TLS crypto backend, aws-lc: sqlx no longer builds rustls with ring (SQLite has no network
  transport), nor its unused `any` and `json` features, and webpki-roots is gone
- Builds use the committed Cargo.lock: every cargo build, test, clippy and llvm-cov run in CI
  (GitHub and GitLab), the release, the Docker image, the `deb:amd64` task and the WASM-guest
  test pass `--locked`, so a manifest change without its lockfile fails instead of resolving
  anew. The release's cargo-zigbuild and ziglang are pinned (0.23.4 and 0.16.0, the pair the
  v1.3.2 aarch64 build used), and GitLab's `:latest` images to the versions they are today
- A release is measured against the Jellyfin release its branch pins (`UPSTREAM_TAG`, the
  latest release when the branch was cut): a newer Jellyfin release no longer refuses the
  release A/B gate (`gate.py`, `gates/run --previous`) or `gates/parity/run.sh --gate`. It is
  noted ("measured against Jellyfin v12.2, the release this branch pins; v12.3 is out") and
  recorded with the oracle in `session.json`, `gate.json`, the verdict JSON, each run's
  `run.json` and the reports. A pin that cannot be read (an `UPSTREAM_TAG` that is not a release
  tag, an empty image pin) is still refused, and so is a pinned image that reports another
  release (`oracle.py matches`). `oracle.py check` no longer takes `--gate`

### Parity
- The session WebSocket's `Sessions` stream is sent when a session starts or ends, playback
  starts or stops, a client reports progress or its capabilities, or a client's session,
  playback or SyncPlay request arrives after its session was idle for more than 10 s, as
  Jellyfin's `SessionInfoWebSocketListener` does, instead of rebuilding and resending the
  session list every period whether or not anything changed. An idle server with the
  dashboard open now sends nothing and builds no session list, and `SessionsStart` sends
  nothing until the first event. The client's period no longer paces the stream: it only
  spaces out low-priority updates (session activity, and automated progress, which Ferrofin
  does not raise yet). A non-administrator's frames list only the sessions they are the user
  or an additional user of, no longer the sessions without a user
- The session WebSocket's `ActivityLogEntry` stream is sent when an activity-log entry is
  created (entries created together share one frame), with `Data: []`, as Jellyfin's
  `ActivityLogWebSocketListener` does: a signal for the dashboard to refetch
  `GET /System/ActivityLog/Entries`. It no longer queries the activity log every period, and
  no longer carries the entries themselves
- The session WebSocket's `ScheduledTasksInfo` stream sends a running task's progress when the
  task reports it, at most once per period, as Jellyfin's `ScheduledTasksWebSocketListener`
  does, instead of reading the task list every period whether or not a task runs. A task's
  start and completion are still sent at once, and subscribing still sends the current task
  list once (Jellyfin sends nothing until a task event, but the dashboard's scan button needs
  it). An idle dashboard now costs no task-list reads
- Report Jellyfin version 12.4.0 (was 12.2.0) in `/System/Info` and `/System/Info/Public`,
  built-in plugin versions and the plugin catalog's `targetAbi` filter: the newest Jellyfin
  release Ferrofin adopts
- Bundle jellyfin-web 12.4 (was 12.2) in the image: the library editor shows an error when
  the library options cannot be saved, Delete All on the Devices page removes every device
  but the current one (it tried to remove only the current one), library lists refresh
  whenever the server reports a played-state change, including one made in another client,
  a library page takes the library's name as its title, the suggestions view's section
  headers no longer link to a broken list, the audio normalization setting shows outside
  iOS again, and `@jellyfin/sdk` 1.0.2 no longer opens the socket without a session
- `POST /LiveTv/TunerHosts` with the id of a configured tuner in another case keeps the id
  as posted, as Jellyfin's `config.TunerHosts[index] = info` does, renaming the tuner and
  its channels' link to it (channel ids and their user data do not change); an id padded
  with spaces no longer matches a tuner, so it adds one
- Open a local M3U playlist or XMLTV guide exactly as configured, as Jellyfin's
  `AsyncFile.OpenRead` does: a `file://` URL is no longer stripped to the path it names,
  so saving a `file://` M3U tuner is refused with `404`
- Decode an XMLTV guide declared `ISO-8859-9` (or `latin5`, `l5` and its other .NET names)
  as .NET's code page 28599 does, with `0x80`-`0x9F` as the C1 controls, not as
  `windows-1254`
- `DELETE /LiveTv/TunerHosts` removes only the tuner, as Jellyfin's
  `TunerHostManager.DeleteTunerHost` does: its channels and their airings stay in
  `GET /LiveTv/Channels` and the guide until the guide refresh it queues removes them,
  where they used to vanish from the guide at once while their items remained (migration
  0046 drops the tuner → channel cascade)
- Read an XMLTV guide as strictly as Jellyfin's `XmlReader`: a mismatched end tag, a second
  root element, text outside the root, a bare `&` or undeclared entity in an attribute or
  in text, a `<` in an attribute, a repeated attribute, `--` in a comment and an XML
  declaration that is not at the start make the guide unreadable (the refresh keeps the stored
  guide), where they were read past; and the guide is the first `<tv>` element wherever it
  is (`ReadToDescendant`), so a well-formed document without one is an empty guide
  (Lineups `[]`, the refresh drains the provider) rather than an unreadable one
- A single genre, studio, person, year, music genre or artist (`GET /Genres/{genreName}` and its
  siblings, and by-name rows with `fields=ItemCounts`) counts only what its viewer may open, as
  Jellyfin 12.2 does: the viewer's libraries, and no alternate version, local version or stacked
  part, whoever asks. A studio or genre also counts the episodes of the series it is written on,
  as the list already did (`/Genres/Action` read `EpisodeCount` 0 where Jellyfin reads 366)
- A server API key with no `userId` is served with no user on `GET /Items`, the by-name lists and
  single routes (`/Genres`, `/MusicGenres`, `/Studios`, `/Artists`, `/Artists/AlbumArtists`,
  `/Persons`, `/Years`) and `GET /Library/MediaFolders`, as Jellyfin does; they answered 400
- A `parentId` naming no item (the zero id included) answers 400 on the by-name lists, `/Years`
  and `/Items/Filters`, as Jellyfin's `GetParentItem` does; they answered an empty 200.
  `/Items/Filters` with a lone `Trailer` or `Program` type answers empty filters, and
  `/Items/Filters2` refuses the zero id
- The server root's (`AggregateFolder`) `RecursiveItemCount` and unplayed count cover the leaves
  in its libraries only, reached through the ancestor closure as Jellyfin does; they counted the
  placeholder row and every person, genre, studio and year row as well
- Find the playlists folder by its type when a playlist is created, as Jellyfin's
  `GetPlaylistsFolder` does, instead of by its path or derived id, and write nothing during
  the lookup. Startup now makes a playlists folder kept at the folder's directory under an id
  of its own a `PlaylistsFolder` under the root, and puts playlists an older Ferrofin saved
  without a parent into the playlists folder
- Let a user named twice in a playlist's `Users` (create, update, or a playlist's stored
  `Shares` read back at startup) edit it when either entry can, as Jellyfin does, instead of
  keeping only the last entry's permission
- Parse `POST /DisplayPreferences/{id}` values as Jellyfin's .NET parsers do: `IndexBy` and
  `chromecastVersion` also take an integer (an unnamed one is kept and written back as its
  number by `GET`), a comma list and padding, and a `chromecastVersion` that does not parse
  is a `400` instead of being stored as stable; the skip lengths, the overlay flag and the
  `homesection` order ignore the white space and trailing NULs .NET ignores
- Accept a playlist `Users` entry without `CanEdit` or `UserId` on `POST /Playlists` and
  `POST /Playlists/{id}`, as Jellyfin does: a missing `CanEdit` shares read-only, a missing
  or null `UserId` is the empty GUID; a `null` `CanEdit` is still a `400`
- Pin the order a display-preferences row's home sections are read in to `Order`, then `Id`:
  on a database adopted from Jellyfin, which keeps every save's rows, `GET /DisplayPreferences`
  answers the newest section of each order, as Jellyfin does. SQLite already returned them in
  that order, but nothing guaranteed it
- Read a display-preferences id as Jellyfin's `Guid.TryParse` reads it: a `(…)`, `{0x…}` or
  space-padded GUID addresses that GUID's preferences, and an `urn:uuid:` id is hashed like
  any other name
- Bind a `POST /Users` body as Jellyfin does: a body without `Name`, or with a null one, is
  a `400` (the name is invalid) instead of keeping the name, and the user's `Policy` in the
  body is validated, so one without both provider ids is a `400` unless it is sent as
  `null`. A body without `Configuration` still resets the user's settings to the defaults,
  as on Jellyfin
- Refuse a `POST /Users/{userId}/Policy` body that leaves out `AuthenticationProviderId` or
  `PasswordResetProviderId`, or sends either blank, with Jellyfin's `400` naming each missing
  member, instead of saving the user's providers as empty strings
- `GET /Items` honours `seriesStatus` (jellyfin-web's TV Status filter), `minOfficialRating`,
  `maxOfficialRating`, `personTypes`, `isPlaceHolder` and `hasOfficialRating`, and
  `hasParentalRating=false` returns only the unrated items, as Jellyfin 12.2's `TranslateQuery`
  does; each used to be ignored. A folder (a series, season or box set) counts as
  rated, or as a placeholder, when an item below it or linked to it is. `maxOfficialRating` can
  lower a restricted user's parental limit but never raise it: Jellyfin lets the request replace
  the limit, and an unrecognized rating clears it there
- `GET /Items` resolves its `studios`, `artists` and `albums` name filters, which replace
  `studioIds`, `artistIds` and `albumIds` as in Jellyfin. Unlike Jellyfin, the lookup writes no
  row; an unknown name matches nothing (Jellyfin answers the whole library for an unknown album)
- On `GET /Items`, an artist's albums listed with no `sortBy` (`artistIds` or `artists`, with
  `MusicAlbum` as the only `includeItemTypes`) come newest first, then by name, Jellyfin's default
  for that request
- `nameStartsWith` on `/Genres`, `/MusicGenres`, `/Studios`, `/Artists` and
  `/Artists/AlbumArtists` is a literal prefix of the sort name, as on `/Items`: `%` and `_` match
  only themselves, and a leading space is kept
- `/Genres`, `/MusicGenres`, `/Studios`, `/Artists` and `/Artists/AlbumArtists` order rows that
  tie on every sort key by item id, as Jellyfin's query does. Equally relevant search hits, and
  names that share a sort name ("The Band" and "Band"), now come back in Jellyfin's order instead
  of name order
- Port Jellyfin's Screen Grabber (`VideoImageProvider`): a movie, episode, music video or
  home video with no Primary image after the local, remote and embedded providers gets a
  frame from its default video stream, 10 % into its runtime (10 s when the runtime is
  unknown), on its first scan or a **Refresh metadata** that searches for missing metadata
  or replaces it. The library's "Screen Grabber" image fetcher gates it, and a new library
  ticks it. A first-scan grab that fails leaves the video to be refreshed again by the
  next scan. `GET /Libraries/AvailableOptions` lists it last for
  the video types, and lists each type's image fetchers in Jellyfin's order (the server-wide
  image fetcher order, then each provider's own order) instead of registration order, so
  the order a new library shows and saves is the order its scans already used
- List each type's remote similarity providers (TheMovieDb, ListenBrainz) before Local
  Genre/Tag in `GET /Libraries/AvailableOptions`, as Jellyfin 12.2 does
- Extract a chapter image, video thumbnail or frame grab the standard way when the
  I-frame attempt fails, as Jellyfin's `ExtractImage` does. A frame or embedded picture
  counts as extracted only when ffmpeg exited successfully and wrote a non-empty file
- Tone-map the frames of HDR10, HLG and Dolby Vision videos to SDR for chapter images,
  thumbnails and frame grabs, as Jellyfin's `ExtractImageInternal` does: with `tonemapx`
  (jellyfin-ffmpeg) when ffmpeg has it, else with `zscale` for all but Dolby Vision with
  no HDR10, HLG or SDR base layer, under the 20 s HDR extraction timeout instead of 10 s
- Port Jellyfin's `FFmpeg:imgExtractPerfTradeoff` startup option as
  `FERROFIN_FFMPEG_IMG_EXTRACT_PERF_TRADEOFF` (`ffmpeg_img_extract_perf_tradeoff` in
  `config.toml`), off by default. On, frame extraction decodes keyframes only and skips
  the 24-frame thumbnail sampling
- Open an M3U tuner's playlist before saving it, as Jellyfin's `M3UTunerHost.Validate`
  does: a missing local playlist is a `404` and a failing playlist URL a `500`, where the
  tuner was saved and only the next guide refresh showed it empty. A Live TV source URL is
  remote when it starts with `http` in any case, so an `HTTP://` playlist or guide is no
  longer read as a file, and a HDHomeRun device's lineup address must be an HTTP URL
- Read XMLTV guides as Jellyfin's `XmlTvReader` does: in the encoding the declaration or
  byte-order mark names (`ISO-8859-1` and `US-ASCII` as .NET decodes them), with a byte
  invalid in it making the guide unreadable; titles, episode titles, descriptions,
  categories and channel names in the provider's preferred language, else the server's
  metadata language; no channel without a name or an id; and an airing whose
  `<previously-shown start>` is its own start is a first showing, not a repeat
- Refuse a `POST /Playlists` body without a `Name`, or with a null or blank one, and an
  `"IsPublic": null`, with `400` as Jellyfin does, instead of creating the playlist; a
  query-string `name` does not rescue such a body. A request with neither a body nor a
  `name` is a `400` too (Jellyfin fails it with a `500`), where Ferrofin created a
  nameless playlist
- Accept a partial body on `POST /DisplayPreferences/{id}`, as Jellyfin does: each member
  left out takes the default of Jellyfin's `DisplayPreferencesDto` (`ShowBackdrop` true,
  250 px images, the rest off or empty) instead of failing with `400 missing field`
- Send every library type's `SimilarItemProviders` and `SimilarItemProviderOrder` in
  `GET /Library/VirtualFolders` even when empty (`[]`), as Jellyfin does, instead of leaving
  them out
- Write a backup manifest's `DateCreated` as Jellyfin writes its `DateTimeOffset`, with a
  `+00:00` offset instead of `Z` (`POST /Backup/Create`, `GET /Backup`,
  `GET /Backup/Manifest` and the archive's `manifest.json`). Archives written with the `Z`
  form still list and restore
- Answer the backup routes as Jellyfin 12.2 does:
  - `POST /Backup/Create` needs its body. With no content type it is a `415`, an empty or
    `null` body is a `400`, and `{}` still takes the defaults. A create during a library
    scan is a `500`, not a `503`.
  - `GET /Backup/Manifest` answers `204` for a file that is not a readable backup.
  - `GET /Backup/Manifest` and `POST /Backup/Restore` reduce the requested name as
    Jellyfin's `SanitizePath` does, so `x.zip/` and `..` are `404`. `POST /Backup/Restore`
    with no content type is a `415`.
  - `GET /Backup` lists the archives in directory order and matches `.zip`
    case-sensitively.
  - Each manifest starts with `ServerVersion`, as Jellyfin's do.
- Report an API key's `DateLastActivity` as `0001-01-01T00:00:00.0000000Z` (.NET's unset
  date) in `GET /Auth/Keys`, as Jellyfin does, instead of the Unix epoch
- Answer a grouped `/Items/Latest` whose first typed library is a movies library as
  Jellyfin 12 does (`GetLatestMovieItems`): the newest `limit` items, one per presentation
  key, then grouped into cards. The "Latest Movies" row is the same in nearly every case.
  The unscoped home row on a server whose first typed library is Movies now reads `limit`
  items instead of five times as many, so a batch of new tracks or episodes can fill it
  with a few cards, and an unscoped request takes half the time (117 → 60 ms on the
  benchmark corpus, one debug build before and after). See
  [docs/UPGRADING.md](docs/UPGRADING.md).
- Keep the playlists folder out of a user's library-wide queries until they can see a
  playlist, and a box-set library until they can see a box set, as Jellyfin's
  `GetUserViews` does. An unscoped recursive `/Items` (or `/Items/Counts`) from a user with
  no playlist no longer lists the folder, which put its counts one above Jellyfin's
- Take a comic's cover from `.cb7` (7z) and `.cbt` (tar) archives as well as `.cbz`, and open
  every comic archive as what its content is, as Jellyfin 12 does, so a `.cbr` that is
  really a ZIP gets its cover too. The fallback page is the first in .NET's key order
  (case ignored first), not byte order. A real RAR `.cbr` still gets no cover
- Give extras their owner's title metadata and dates, as Jellyfin's `RefreshExtras`
  does. Trailers, featurettes and other extras take the movie's or series' genres,
  studios, production locations, overview, ratings, year and premiere date when a
  scan finds them new, renamed or gone, or on a full refresh, and the year and
  premiere date on every scan. A video extra also takes the year in its file name until
  its owner has one. Existing extras need one "Search for missing metadata" refresh to
  get the rest; see [docs/UPGRADING.md](docs/UPGRADING.md)
- Bundle jellyfin-web 12.2 (was 12.0) in the image: admins get a user picker on the Quick
  Connect page, "Still watching?" is off by default, the client sends `Accept-Language`,
  the save-subtitles-with-media option shows for movie, TV and mixed libraries, and
  embedded VobSub subtitles render again
- Report Jellyfin version 12.2.0 (was 10.11.8) in `/System/Info`, built-in plugin versions
  and the plugin catalog's `targetAbi` filter; jellyfin-sdk-kotlin treats servers below
  12.0.0 as unsupported. The route gate stays on the 10.11.8 contract. Emby-era credentials
  (`X-Emby-Authorization`, `X-Emby-Token`, `api_key`) still need `EnableLegacyAuthorization`;
  see [docs/UPGRADING.md](docs/UPGRADING.md)
- Serve `GET /Items/{itemId}/Collections` (Jellyfin 12.1's `GetItemCollections`): the
  collections the user can see that include the item, sorted and paged as upstream.
  jellyfin-web's item page calls it and hid its Collections section on the old `404`.
  `contract_superset` now probes every 12.1 operation the 10.11.8 contract lacks
- Honour Jellyfin 12.1's `audioLanguages` and `subtitleLanguages` filters on `GET /Items`
  and `GET /Trailers`: an item matches when its own streams or an alternate version's
  carry a listed language (exact, case-sensitive codes; `und` also matches a stream with
  no language), and a folder or collection matches when such an item sits below it. As
  upstream, every media-stream filter (these two, `hasSubtitles`, `isHd`, `is4K`, `is3D`,
  `videoTypes`) now keeps alternate versions in the query, and the legacy
  `GET /Users/{userId}/Items` ignores both lists
- Publish Jellyfin 12's `AudioLanguages` and `SubtitleLanguages` facets on
  `GET /Items/Filters2` for movie and TV type sets: each distinct stream language as
  `{Name: "English (eng)", Value: "eng"}`, alternate versions included, show and season
  sets widened to their episodes, sorted by name in the request culture. ISO 639-2-only
  codes (`und`, `mul`, `zxx`, …) now resolve as in Jellyfin 12, so `/Localization/Cultures`
  and the metadata editor's language list carry all 496 languages (was 194). Clients see
  this in subtitle menus: a subtitle track tagged `und` is now titled "Undetermined" (was
  "Und"), and `mul`, `zxx` and `mis` tracks read "Multiple languages", "No linguistic
  content" and "Uncoded languages". External subtitle files named with these codes
  (`movie.und.srt`, `movie.mul.srt`) now have them read as the track language.
  `GET /Items/Filters` is unchanged
- `GET /Items/Filters2?isAiring=false` lists the filters of the programmes not on now
  (starting later or already ended), as Jellyfin 12 does. It returned an empty filter set,
  because the predicate required a programme to start after now and also end before now
- Serve Jellyfin 12's `MediaStream.IsOriginal` (ffprobe's `original` disposition, always
  written), `LocalizedLanguage` and `LocalizedOriginal` (on every audio stream). An
  original audio track's `DisplayTitle` ends in " - Original" (localized). As upstream, a
  stored ISO 639-2/B language is sent as its /T code (`ger` → `deu`, `fre` → `fra`), a
  video track with a title appends its tags with " - " (`Main - 1080p - H264 - SDR`, was
  `Main 1080p H264 SDR`), and Live TV channel, recording and opened live streams carry the same
  localized labels as library items, plus the `DisplayTitle` they previously lacked
- Send Jellyfin 12's `BaseItemDto.OriginalLanguage` on every item that has one, with no
  `Fields` request needed and no series inheritance, and accept it in the metadata editor
  (`POST /Items/{itemId}`; blank clears it, as does an edit that omits it). As upstream, a
  known language is stored and served as its two-letter code: OMDb's "English" becomes
  `en`, an edited `fra` becomes `fr`, and rows already stored as "English" are sent as
  `en`. The NFO `<originallanguage>` element now follows `<originaltitle>` and is skipped
  when blank
- `hasSubtitles`, `isHd`, `is4K` and `minWidth`/`minHeight`/`maxWidth`/`maxHeight` judge a
  merged item by its whole version group, as Jellyfin 12 does: a movie whose subtitles or
  4K file live only on its second version matches, listed under the primary, and an SD
  primary with a better version no longer counts as SD. `hasSubtitles`, `isHd` and `is4K`
  also match a series, library or collection by the items below it
- Port Jellyfin 12.0's data routines: linked-children move, orphaned extras, OwnerId
  relationships, version links, `GetCleanValue`, forced sort names, localized user-view
  consolidation
- Port Jellyfin 12.1's routines and rules: `RepairAlternateVersionLinks` (primaries re-derived
  from `LinkedChildren`), `StripEmbeddedLinkedChildren`, the re-dated rating-level pass with
  12.1's `GetRatingScore` (whole value first, unrated parts skipped, case-insensitive tables),
  the user-view consolidation's reference moves, and `ApplyAlternateVersionFiltering` (a
  version is hidden only behind a primary that exists in the same library)
- Adopting a Jellyfin database re-attaches each extra whose owner is gone the way Jellyfin
  does: to the lowest-Id video in the extra's folder, matched literally and ignoring case in
  any script. An `_` or `%` in a folder name no longer matches other folders, and an extra
  with no folder is left without an owner instead of joining an arbitrary video
- Port Jellyfin's `PlaylistResolver`: a library scan makes a playlist of each `.m3u`,
  `.m3u8`, `.pls`, `.wpl` and `.zpl` file in a music or untyped library (named after the
  file, under the album, artist or folder item it sits in, `.pls` no longer a song) and of
  each `[playlist]` folder or folder holding a `playlist.xml` (nothing inside it is an item). Members come
  from the file's entries in order — relative, absolute or `../`, legacy encodings detected —
  or from the folder's `playlist.xml`, and follow `PlaylistMetadataService`: read when the
  playlist is new or its file changes, merged on a full refresh that keeps metadata. Such a
  playlist is open to everyone and owned by nobody, so it is read-only. Where Jellyfin 12.2
  writes a `<name>.xml` sidecar beside a playlist file, that sidecar hides the entries and
  the playlist stays empty; Ferrofin does not port that, and does not take such a sidecar
  for a folder's `playlist.xml` (#56)
- Create the internal playlists folder at startup, as `CreateRootFolder` does: a server
  whose only playlists come from the library scan now lists the Playlists view
- Check the server directories at startup, as Jellyfin does. Before a scheduled restore is
  applied or the database is opened, Ferrofin creates its configuration, log, plugins,
  data, cache, `<data>/data` and `<data>/root` directories, and leaves Jellyfin's
  `.jellyfin-<kind>` marker in each (the cache also gets `CACHEDIR.TAG`). A layout that
  used to start now stops the boot with a clear error when a directory holds another
  kind's marker (two of its paths point at one directory) or cannot be created or written
  to. See
  [docs/UPGRADING.md](docs/UPGRADING.md)
- The Playlists view lists every playlist the user may open, wherever it is, as Jellyfin
  12.2 does — not only the ones made in the app
- Port Jellyfin 12.2's `HasVisibleItem` for the Playlists and Collections views in
  `GET /UserViews`: a view is listed when the user can see at least one playlist (or box
  set) anywhere in their libraries, so a playlist inside a music album now shows the
  Playlists view, and another user's private playlist no longer does
- Port the rest of `Playlist.IsVisible`: Jellyfin 12.1's `HasParentalAllowedChild` (a user
  with a maximum parental rating no longer sees a playlist whose members all exceed it) and,
  for a playlist file, its own rating and tags. It applies in browsing, the Playlists view and
  every `/Playlists/{id}` endpoint
- `GET /Playlists/{id}/Items` leaves out the members the user may not see, as upstream's
  `GetPlaylistItems` does (`GET /Playlists/{id}` still lists every member in `ItemIds`), and
  its `userId` parameter names another user only for an administrator (`403` otherwise). A
  move counts positions among the members the user sees (`MoveItemAsync`)
- Per-field metadata locks (`LockedFields`, stored in `BaseItemMetadataFields`): the
  metadata editor's checkboxes round-trip, and scans and refreshes keep a locked field
  as the user set it. Editing an item no longer locks it; `LockData` is exactly what the
  editor sends, and a lock change on a folder reaches every descendant and linked member.
  A series, season or album edit passes its rating and tag changes to its children, each
  child's own field locks honoured. A locked item still has its local artwork validated
  (a new `poster.jpg` is found). See [docs/UPGRADING.md](docs/UPGRADING.md) for items
  locked by earlier versions
- Fetcher choices follow Jellyfin's gate for every kind, in scans and single-item refreshes:
  a kind a library saved no fetcher choices for, and an item in no library (an artist known
  only by name), now uses the server-wide metadata options — their disabled fetchers and
  their order. Their defaults (Jellyfin's) disable TheAudioDB metadata for albums and
  OMDb for music videos. A music artist with no saved choices may use every metadata
  downloader, as in Jellyfin 12.2. See [docs/UPGRADING.md](docs/UPGRADING.md)
- TheAudioDB descriptions return for music artists and albums, in the item's metadata
  language when TheAudioDB has it, else in English
- OpenSubtitles downloads share one login, as the plugin does. The token is kept until it
  expires or the account changes (it was renewed for every subtitle). A rejected token is
  renewed and the download retried once. The account's daily quota is tracked: once it is
  spent, downloads are refused ("OpenSubtitles download limit reached") without asking
  OpenSubtitles until its reset time, and the remaining count is logged with each
  download. A download without an account is refused before any request, and the error for
  a download OpenSubtitles rejects carries its reply (a web page as `[html]`), as the
  plugin's does
- Honour "Date added behavior for new content" (Dashboard → Libraries → Display):
  "Use date scanned into the library" dates a new item by the moment Ferrofin first
  detects it, by a library scan or the disk watcher/webhook alike, and its series'
  date added follows; "Use file creation date" (the default) keeps the file's
  creation time, and re-dates a file whose modification time changed, as Jellyfin
  does. An `.nfo` `<dateadded>` or a photo's EXIF date now also re-dates an item
  already in the library whenever it is read again. Adopting a Jellyfin install
  carries the choice over from `metadata.xml`, including an install adopted by an
  earlier version (on its first boot of this one). Existing items otherwise keep their
  stored date. See [docs/UPGRADING.md](docs/UPGRADING.md)
- Honour `HlsAudioSeekStrategy` (Dashboard → Playback → Transcoding): when set to
  "Prevent audio copy on HLS seek", a seek into a stream that was direct-copying audio
  forces re-encoding so the audio starts at a keyframe boundary, eliminating the pop/click
  some clients produce on partial-GOP audio. As upstream, the default is off
- Cut remuxed MKV HLS playlists on the file's own keyframes, as Jellyfin does. They are read
  from its Matroska cues on the first request and stored. The Keyframe Extractor task fills
  the same store, trying the Matroska cues before ffprobe, and
  `AllowOnDemandMetadataBasedKeyframeExtractionForExtensions` takes effect. A seek restarts
  the transcode at the segment's playlist time, half a second later for a copied video so
  ffmpeg lands on the keyframe itself
- Honour `SubtitleExtractionTimeoutMinutes` (Dashboard → Playback → Transcoding): an
  embedded-subtitle extraction that exceeds the configured timeout is killed and its
  partial output cleaned up. A drop guard ensures cleanup even on cancellation. As upstream,
  the default is 30 minutes
- Stop playback a client abandoned, as Jellyfin does: a session playing without a progress
  report for five minutes is stopped at the client's last position, and with
  `InactiveSessionThreshold` set, a session paused past it is told to stop. A client that
  vanished mid-playback no longer stays "playing" and no longer makes transcode cleanup and
  database optimization skip every run
- Port `TrimCopiedAudio` noise bitstream filter: when a video is being transcoded but the
  audio is direct-copied, ffmpeg 5.0+ trims the leading audio silence to match the
  requested seek point, eliminating the A/V desync on mid-file seeks into remuxed HLS. The
  `noise=drop='lt(…)'` BSF and `aac_adtstoasc` are combined into a single `-bsf:a` argument
- Port subtitle extraction failure handling and ASS font fix-up from upstream
  `MediaEncoder`: extraction failures are logged at WARN with the ffmpeg output and do not
  abort the transcode; ASS subtitles with missing `[Fonts]` or `[Graphics]` sections get
  the headers appended on write so libass renders them correctly
- Trickplay frames and tiles are made in the server's temp directory (`{cache}/temp`), as
  Jellyfin makes them in its own, instead of the system's `/tmp`. "Clean Cache Directory"
  sweeps what a killed generation leaves there, and a small or read-only `/tmp` no longer
  stops trickplay
- Port `DownMixStereoAlgorithm` filter table: the 15-entry `(algorithm, channel_layout) →
  pan=stereo|…` lookup for Dave750, NightmodeDialogue, Rfc7845 and Ac4 algorithms, with
  7.1 → 5.1 pre-downmix chains and channel-layout inference from stream metadata. When
  the algorithm's filter is active, `-ac 2` is suppressed (the `pan=` filter sets the
  channel count)
- Serve Jellyfin's `/health`: plain-text `Healthy`, or `Unhealthy` with a 503 when the
  database does not answer, and behind a base URL a redirect from `/health` to
  `{base}/health`, so healthchecks written for Jellyfin keep working. `/health/ready` runs
  the same database check, so the chart's readiness probe now fails while the database is
  unreachable (it was always ready before)
- The startup wizard's "Allow remote connections" choice applies at once, as in Jellyfin:
  `POST /Startup/RemoteAccess` now saves the network configuration the way the dashboard
  does, so the live remote-access policy takes it without a restart
- Serve HTTPS from the dashboard's Networking settings, as Jellyfin does: with **Enable
  HTTPS** and a PKCS#12 certificate that loads, the server also listens on
  `FERROFIN_HTTPS_PORT` (8920) and publishes `https://` addresses; **Require HTTPS**
  redirects plain HTTP there with a `307`; a new certificate path flags a restart, and a
  missing one is answered `404`. `X-Forwarded-Proto` now counts only from known proxies,
  also for addresses published from each request; see [docs/UPGRADING.md](docs/UPGRADING.md)
- Music libraries keep their folder tree: a folder that is neither artist nor album (a
  grouping folder such as `Rock/`, an artist's `albums/` or `live/`, a folder of playlists)
  is a folder item, as in Jellyfin, and what it holds sits under it in folder browsing
  instead of being lifted to the level above. The next scan moves existing items under
  their folders; their ids, play state and playlist entries stay
- A series refresh removes its missing episodes once you have them, as Jellyfin 12.2 does:
  a missing episode whose number a file covers, one with no number, and one carrying the
  provider id of an episode you have under another number are removed, so an adopted
  library no longer lists those episodes twice (see `docs/UPGRADING.md`)
- Missing and unaired episodes from TheMovieDb, as Jellyfin 12 offers them: the TMDb
  settings page's "Create unaired episodes", "Create missing episodes" and "Include
  specials" options, its library list, refresh interval and grace period, and the "Refresh
  upcoming and missing episodes (TheMovieDb)" scheduled task. Off by default
- Convert subtitles with Jellyfin 12's writers (SubtitleEdit's libse 4.0.12), byte for byte:
  converted WebVTT, SubRip, SSA and ASS lose the byte-order mark, and WebVTT loses the fixed
  region block and `line:90%` on every cue. An `{\anN}` sign is placed where it asks to be
  (`{\an8}` at the top), other SSA/ASS override tags are removed instead of shown as text,
  and `<font color>` becomes a WebVTT colour class. An ASS script converted to SSA keeps its
  styles and comments. `Stream.ttml` now serves TTML (`application/ttml+xml`; it was a 500).
  An SSA script's italic, bold, underline and colour tags reach every format as HTML tags.
  Unlike Jellyfin, a script's `Comment:` events never show up as subtitle lines. A WebVTT
  file converted to ASS does not get Jellyfin's style conversion yet
- `/System/Configuration/{key}` knows Jellyfin's named configurations only: `branding`,
  `encoding`, `network`, `metadata`, `xbmcmetadata`, `livetv` and `database`. Reading any other
  key is a 404, as in Jellyfin (it was a 501, or a file a save had left behind), and saving one
  fails as Jellyfin's does instead of writing a file. `database` reads as the SQLite provider
  Ferrofin runs on, and saving it is refused (400), since no other provider can be used
- `GET /Items/Counts` answers `ItemCount` with the number of items the query matched, of
  every type, as Jellyfin 12 does. It always answered 0, which only Jellyfin 10.11 did
- `GET /Genres`, `/MusicGenres`, `/Studios`, `/Artists`, `/Artists/AlbumArtists` and the
  genre facet of `GET /Items/Filters2` filter their content by kind as Jellyfin's query
  does. An `includeItemTypes` of only Live TV kinds with no stored rows (`Program`,
  `Recording`, `ChannelFolderItem`) lists nothing; it listed every value.
  `excludeItemTypes` counts only when no kind is included, so naming one kind in both lists
  answers that kind instead of nothing
- `GET /Artists` and `/Artists/AlbumArtists` honour `studios` (pipe-delimited studio names):
  the named studios replace `studioIds`, and since an artist carries no studios the list is
  empty, as in Jellyfin. The parameter was dropped and every artist listed. Unlike Jellyfin,
  an unknown name does not create a studio
- `GET /Library/VirtualFolders` and the libraries of `GET /System/Info/Storage` come in the
  order the file system lists the libraries folder, as in Jellyfin, instead of sorted by
  name, and each library's `Locations` are sorted in the request's language, so
  `/media/kids` comes before `/media/Movies`. A library folder that is a symbolic link to a
  directory is listed (and scanned) as in Jellyfin; it was skipped. A media path an adopted
  Jellyfin library stores as `%AppDataPath%/…` or `%MetadataPath%/…` is served and compared as
  the real path in `LibraryOptions.PathInfos`, so removing that path from the dashboard
  removes it from the library's options too. A library shortcut (`.mblink`) written with a
  byte-order mark or in UTF-16 resolves as Jellyfin reads it, and one that cannot be read is
  logged with its path instead of vanishing silently
- A media folder two libraries share takes its library options (metadata language and
  country, saving artwork beside media and the rest) from the first of them by sort name, as
  in Jellyfin; it took them from the last. The metadata language and country of an item in
  nested libraries now come from the innermost one, and `GET /Items/{itemId}/Ancestors`
  names the first library by sort name when two list the same folder
- Library tiles are drawn as in Jellyfin 12.2. An image at the root of a library's media
  folder (a `folder.jpg` in `/media/movies`) becomes the library's image, after one in the
  library's own folder. A music library's collage comes from the artists its tracks credit,
  not its albums, and every collage samples 8 items that have a Primary image; a library
  whose items have none keeps the icon tile, where Ferrofin drew from any item. A library's
  or the root's first refresh at the default mode, or one past the library's automatic
  refresh interval, redraws the tile or the root's image, and the library and the root now
  record their last refresh
- Library renames and media-path removals compare paths as Jellyfin does, ignoring the case
  of every letter, not only ASCII: on a case-insensitive file system, renaming `élan` to
  `Élan` works instead of answering `409`, and the users' library preferences follow a
  library whose stored path differs in a non-ASCII letter's case. Users' library and view
  preferences are read as .NET parses a GUID: a padded or `(…)` value counts, a
  `urn:uuid:` one does not
- An extra (a trailer, featurette or theme song) is never looked up by a WASM plugin's
  metadata or artwork source, as Jellyfin 12 never asks a remote provider about one: it has
  no identity of its own online, so a lookup by its file name filled it with another title's
  overview, people or Primary image. Its local readers, embedded images and extracted frames
  still run. The item it belongs to is asked as before

### Performance
- `/Persons` with a favourite filter: build the user's favourite-person set once per
  statement, as Jellyfin does, instead of probing it again for every person row. On the
  benchmark corpus (one debug build, before and after; the figures below too) that is
  6.1 s → 7 ms, and `isFavorite=false` (past a minute) → 9 ms
- `/Artists` and `/Artists/AlbumArtists` with `mediaTypes`: keep the media-type filter
  from driving the artist lookup (12.7 s → 32 ms for `mediaTypes=Video`). Skip the
  played and total counts an artist page never shows, as Jellyfin does (plain `/Artists`
  87 → 37 ms)
- Recursive `/Items` sorted by `Artist`, `AlbumArtist` or `Studio`: read each item's own
  values for the sort key (4.0 s → 0.26 s)
- The per-user visibility check on by-id requests (Jellyfin's `GetItemById(id, user)`) reads
  the item and the chain above it in one statement, and keeps users' permissions and
  preferences and the libraries' rows and options in memory, dropped by every write to them.
  It had added about six statements to every by-id request and read a user's policy up to
  five times in one. Against 1.3.2, the release A/B gate's regressed cells went from 26 to
  11, and an item's intros and images are back within noise of it.
  `FERROFIN_ROW_CACHE_TTL_SECS` and `FERROFIN_AUTH_CACHE_TTL_SECS`
  (5–300, default 30) bound how long a change made outside the server, in the database
  itself, goes unseen
- The libraries' physical folders and `Enabled` option come from that same in-memory library
  set, and the Live TV view is found among the views the user scope has already read. The
  home screen's `/Users/{id}/Views` resolved a library's physical folders three times and
  looked the Live TV view up once, a statement each, and read every library's options file;
  every query scoped to the user's libraries read the files and the Live TV view too. On the
  benchmark corpus (release builds before and after, requests interleaved) the views went
  from 21 statements to 17, as in 1.3.2, and from 1.11 to 0.95 ms. A library's options file
  or row edited outside the server is now seen by these reads within
  `FERROFIN_ROW_CACHE_TTL_SECS`, as the per-item check already saw it; changes made through
  the server still apply on the next request
- A folder's played and total leaf counts read one folder at a time (a series' or season's
  detail page, a box set or playlist, a plain folder, and any folder a list page's batch
  leaves out) are one statement, as Jellyfin counts them, and test each leaf's played state
  on its own rows instead of building the user's whole watch history first. On the
  benchmark corpus (release builds before and after, requests interleaved) a series' two
  counts went from 0.61 to 0.15 ms, and a 7,490-leaf folder's from 34 to 19 ms

### Security
- A server whose setup was completed no longer starts when its database is missing, empty or
  has no migration history, as Jellyfin 12.2 refuses. It created an empty database and seeded
  a new `admin`; without `FERROFIN_ADMIN_PASSWORD` that admin had no password, so anyone who
  could reach the server signed in as an administrator with an empty password (a lost mount, a
  wrong data directory or a deleted file was enough). The refusal names three ways out: restore
  the database, set `IsStartupWizardCompleted` to false in `system.json` to start over, or
  start with empty configuration and data directories. See [docs/UPGRADING.md](docs/UPGRADING.md)
- `GET /System/Configuration/livetv` no longer hands Live TV credentials to users who are not
  administrators: each tuner's URL (an IPTV playlist's usually carries the account's user name
  and password) and each guide provider's password (the Schedules Direct password hash) come
  back blank for them. Jellyfin serves both to every signed-in user; only the dashboard's admin
  pages read them, so jellyfin-web is unaffected
- A signed-in user can no longer take over another user's session by sending that user's
  `Client` and `DeviceId` headers. Sessions are now keyed by client, device and user, as in
  Jellyfin 12. Before, the other user's session, WebSocket included, was handed to the caller,
  who could then remote-control that player without `EnableRemoteControlOfOtherUsers`. Session
  ids now match Jellyfin's byte for byte, so each one changes once on upgrade; they live only
  in memory and clients take whatever id the server reports
- A user's details no longer say whether the account has a password. `HasPassword` and
  `HasConfiguredPassword` are `true` for every user, as Jellyfin 12 reports them, so the
  anonymous login-screen list (`GET /Users/Public`) no longer tells anyone which accounts have
  no password. Signing in with an empty password still works for those accounts. See
  [docs/UPGRADING.md](docs/UPGRADING.md)
- Taking a device away from a user now signs that device out, as Jellyfin 12.1 does. When a
  user's policy is saved without "Allow access from all devices" and a device the user is signed
  in on is not among the allowed ones, its token is revoked at once. Before, it kept working
  until the device signed in again; only new sign-ins from it were refused. A rename runs the
  same check. If the check fails, the save still succeeds and the error is logged
- Behind a proxy listed under **Known proxies**, a client can no longer choose its own address
  through `X-Forwarded-For`. When the proxy writes a token that is not an address for its client
  (Squid's `unknown`, for example), the client is now that proxy, as in Jellyfin. Before, the
  token was skipped and the entry to its left, which the client wrote itself, became the
  client's address: enough to get past a remote IP filter's allow or block list. Quoted entries
  are read too, as Jellyfin reads them
- A library scan's `LibraryChanged` push reaches each signed-in user through the per-user
  filter, as in Jellyfin, instead of one payload sent to every session: a user no longer
  receives the ids of items added to or changed in libraries they cannot open, nor of those
  libraries' folders. Removed ids still reach every user, as Jellyfin sends them, so a deleted
  item leaves every screen. The push now also names the items a rescan saved (`ItemsUpdated`), the
  by-name artists a scan retires (`ItemsRemoved`), and, from the TMDb missing-episodes pass, the
  virtual seasons it adds but not its pathless episodes, as Jellyfin's `FilterItem` does. It
  goes out `LibraryUpdateDuration` after the scan announces it, like every other library
  change. WASM plugins receive every batch of library changes as `LibraryChanged`
- The `LibraryChanged` push tells each user about an added or changed item only when they may
  see it on its own, as Jellyfin's `IsVisibleStandalone` check does: another user's private
  playlist, or an item above the user's parental rating or behind a blocked tag, is no longer
  named to them, nor a folder they cannot open
- Adopting a Jellyfin 12.2 database keeps the parental score that unrated episodes and seasons
  carry from their series. The first boot re-ran Jellyfin's rating-level migration, which 12.2
  had already run, and cleared it, so a user with a maximum parental rating saw those episodes
  in lists, counts, Latest, Resume and Next Up until the next full library scan
- WASM plugins: what a plugin's `/Plugins/{id}/web/…` route answers is served on the server's
  own origin, where jellyfin-web keeps every user's token, so the server now owns the headers
  that decide what a browser lets it do. Every answer carries `X-Content-Type-Options: nosniff`
  and a sandboxing `Content-Security-Policy` (an HTML page the plugin serves runs in an opaque
  origin and cannot read stored tokens), and the plugin can no longer set cookies or clear the
  site's storage. The caller's identity reaches the plugin only when Jellyfin's default
  authorization policy admits them: a disabled administrator was passed on as an
  administrator, and a remote caller without remote access or a user outside their access
  schedule as signed in
- WASM plugins: a `.wasm` dropped into the plugins folder by hand whose id belongs to a
  compiled-in plugin (OpenSubtitles, TMDb and the other in-tree providers, Intro Skipper, File
  Transformation, Merge Versions) is no longer loaded. It ran every task, provider and route
  it declared under the compiled-in plugin's enabled switch and was handed that plugin's
  settings (the OpenSubtitles user name and password among them), while `/Plugins` listed
  only the compiled-in one
- WASM plugins: one plugin file can no longer drop every other plugin for a boot. A name with
  a NUL byte panicked the loader (the name became the plugin thread's name); names that are
  blank, longer than 256 bytes or carry control characters are now refused, a panic while
  loading one file skips only that file, and install checks a plugin the same way the load
  does, so one that installs is not skipped at the next boot for an invalid default
  configuration or name
- WASM plugins: a web transform can no longer make the server run out of memory from anonymous
  `/web` requests. A blank search text is refused (it matched between every character), and a
  transform that would grow a file by more than 256 KiB leaves the file unchanged
- WASM plugins with a semver prerelease or build version (`0.2.0-beta.1`, `1.0.0+git.abc`) can
  be enabled, disabled and uninstalled from the dashboard; those buttons answered `400`
- `POST /Jellyfin.Plugin.OpenSubtitles/ValidateLoginInfo` requires the subtitle management
  permission, as the plugin does: any signed-in account could make the server try
  OpenSubtitles logins of its choosing with the shared application key. It now answers as the
  plugin does, `{"Downloads": n}` for a working account (logging the test session out again)
  and `401 {"Message": …}` for a refused one, where it answered an empty 200 or `{"error": …}`
- A host name in `KnownProxies` (a docker-compose service name, say) is resolved, as in
  Jellyfin, instead of dropped without a word. Behind a proxy named that way every client
  looked like the proxy, a LAN address, to the remote-access, IP-filter and local-access
  checks. An entry of a disabled address family is dropped and an IPv4-mapped one unmapped, as
  Jellyfin does, ASP.NET's loopback defaults (`::1`, `127.0.0.0/8`) stay trusted whenever the
  list is not empty, and an entry that resolves to nothing is logged once at warning level
- A line break in a logged value (the `DeviceId` a client sends, a WASM plugin's log text) can
  no longer forge a line in the log file behind `/System/Logs`: the file continues the entry
  on an indented line instead (a carriage return is written as `\r`), and a plugin's text is
  logged escaped, under `guest_message`, cut at 8 KiB
- WASM plugins: an `http-fetch` to an IPv6 address that carries a private IPv4 one is refused
  like the IPv4 address itself, unless the plugin is granted private-network access. A NAT64
  (`64:ff9b::/96`), 6to4 (`2002::/16`) or IPv4-compatible (`::a.b.c.d`) address reached, say,
  the cloud metadata service or the server's own loopback through a gateway, where only the
  IPv4-mapped form was unwrapped. The local-use NAT64 prefix `64:ff9b:1::/48` is refused
  outright
- WASM plugins: the engine grants a plugin only the WebAssembly features the host needs. A
  plugin could use shared memory (threads), GC types, exception handling and the async
  component model, which wasmtime turns on with its default build; a plugin that uses one no
  longer loads, and none known does. The server also no longer builds wasmtime's profiling,
  compilation cache, core dump and pooling allocator support, or WASI preview 1 and 3
- File Transformation: what the pipeline serves for one `/web` file is capped at 16 MiB, so an
  anonymous page load can no longer make the server allocate without bound. A callback
  endpoint's answer is read no further than that, and a step that would grow the file past it
  (an admin search/replace whose replace text multiplies the file, or another plugin's
  transform) is skipped with one warning, leaving the file as it was before that step
- Dependencies with published RustSec advisories are upgraded: wasmtime and wasmtime-wasi
  49.0.0 → 49.0.4 (25 advisories against the WASM plugin host, among them a WASI symlink-rename
  sandbox escape, an aarch64 Cranelift miscompile and fuel-limit bypasses), zip 8 → 9 (three
  extraction advisories), h2, tokio, ctutils, event-listener, webpki-root-certs (distrusted CA
  roots), and quinn-proto, which Cargo.lock records although nothing builds it
- A supply-chain gate: CI's `dependencies` job runs `cargo deny` (RustSec advisories, licenses
  compatible with GPL-3.0-only, unused workspace entries, crates.io sources only),
  `cargo machete` and `scripts/check-workspace-deps.py` (no inline dependency versions) on every
  push and PR, and a release re-checks the advisories before it is tagged
  ([CONTRIBUTING.md](CONTRIBUTING.md) says what to do when it fails)

### Bug Fixes
- The web client's Next Up row loads again: jellyfin-web 12.x sends `nextUpDateCutoff` as a
  bare date, which Ferrofin answered with a `400`. Every `DateTime` query parameter now binds
  as Jellyfin's does: a date or an offset-less date-time is accepted and read as server
  time (`nextUpDateCutoff`, activity-log `minDate`/`maxDate`, `datePlayed`, and the
  `GET /LiveTv/Programs` window, which read such values as UTC before), and `datePlayed` also accepts
  the legacy `yyyyMMddHHmmss` form as UTC
- Seeking or resuming an HLS stream whose video is transcoded and audio copied (an HEVC
  movie on an H.264-only client) plays again: the copied-audio trim cut at the absolute
  seek time on timestamps that start at the seek, so every audio packet was dropped, fMP4
  segments came out empty ("Source error" in Jellyfin for Android) and TS segments had no
  audio. The cut now counts from the seek, and the audio starts with the video
- A Live TV tuner, guide provider, series timer or timer whose stored settings no longer
  parse is logged (a warning naming its table and id, once) instead of vanishing silently,
  and a `livetv.json` that cannot be read or parsed logs a warning naming it once per change
  before the defaults are used; a timer for a programme is found past a broken one
- A backup no longer reports success with something left out:
  - a file it cannot read, or whose name is not UTF-8, fails the create with a `500`,
    and the server log names the file;
  - with the metadata on a custom path, the default metadata folder goes into the archive
    too (`metadata-default`, as in Jellyfin) and is restored there;
  - a backups folder that cannot be read fails `GET /Backup` instead of listing nothing;
  - the database is taken only from its consistent snapshot.
- Two backups created in the same second no longer replace each other: the second one's
  archive takes a `-1` suffix
- "Optimize database" no longer runs while a database backup is copying, and a backup asked for
  while it runs waits for it to finish. Its vacuum and truncating checkpoint stalled every
  database write for up to 30 seconds when the two overlapped
- A Live TV channel that drops out of the tuner lineup and comes back keeps its users'
  favourites, likes and play state, as in Jellyfin. The guide refresh that recreates the
  channel gives back the data its removal had set aside
- Book metadata from an EPUB's or a sidecar's OPF follows Jellyfin's reader:
  - the first description, publisher, language, date and identifier of each kind count;
  - the first Calibre series, series index, rating and sort title count;
  - Calibre's series index and rating round half to even (a `2.5` series index is book 2);
  - a cover named by `<meta name="cover">` is also found under `Images/`.

  Affected books change on their next metadata refresh
- A genre whose movies and series have no poster no longer gets an image drawn from music
  art of the same genre name, as in Jellyfin, where only movie and series posters are
  sampled. An image already drawn that way stays until deleted. See
  [docs/UPGRADING.md](docs/UPGRADING.md)
- "Search for missing metadata" or "Replace all metadata" on a genre, music genre, playlist,
  photo album, folder, library or the root folder now redraws the image Ferrofin generated
  for it from the current artwork, as in Jellyfin. Before, only a library scan drew these
  images, and only when one was missing or out of date. A library scan now also redraws a
  photo album's, folder's or playlist file's image whenever its own refresh runs every
  provider (its first refresh, a changed folder, or its library's automatic refresh interval
  coming due), a library's or the root's refresh redraws a library tile older than a week, and
  a locked item's image is left alone
- The "Clean up collections and playlists" scheduled task is removed, as in Jellyfin 12. It
  ran at every start and removed each collection and playlist member whose file it could not
  see, so a boot with the media storage not yet mounted emptied every playlist and collection
  for good. Deleting an item still removes it from them. See
  [docs/UPGRADING.md](docs/UPGRADING.md)
- A library on a database adopted from Jellyfin no longer browses empty after Ferrofin's
  first scan, and media deleted from disk is removed from it again. Jellyfin files a
  library's items under the library location's folder, Ferrofin under the library itself.
  Browsing, counts, Latest, Next Up and the scan's removal of deleted media (including a
  watcher or webhook event's) now read both. A scan never removes a library's own folders,
  nor an entry whose file or folder is still on disk unless a new entry for the same file
  replaced it. On an adopted database the first scan after upgrading removes the entries
  for media deleted from disk since Ferrofin first scanned the library (or since adoption,
  if it was never scanned), with their watched state. Until multi-version grouping is
  ported, a movie's alternate versions (`Movie - 1080p.mkv` beside `Movie - 2160p.mkv`)
  are listed as separate movies once the library is browsable again, and a plain
  subfolder of a movie library shows as an empty folder. See
  [docs/UPGRADING.md](docs/UPGRADING.md)
- A movie's alternate version on a database adopted from Jellyfin 10.11 takes its primary's
  official rating (scored), overview, community rating, genres, year, images and provider ids
  when it is re-homed at boot, as Jellyfin 12's first scan gives it; it kept what 10.11 last
  copied, so a rating changed since then stayed on the primary alone. A boot that fails or stops
  before that copy makes it on the next boot
- The `/socket` WebSocket authenticates like every other request, as Jellyfin 12.2's does:
  Emby-era credentials (`api_key`, `X-Emby-Token`, `X-MediaBrowser-Token`) count only with
  `EnableLegacyAuthorization`, and a connection without valid credentials is refused with
  `403` instead of opening. A socket that authenticates with the `Authorization` header, as
  jellyfin-sdk-kotlin clients' sockets do, now joins its session, so remote control and SyncPlay
  reach those clients (#75). Opening a socket no longer blanks the session's remote address,
  so the dashboard and the session activity entries show the client's IP. See
  [docs/UPGRADING.md](docs/UPGRADING.md)
- `GET /Persons` sorts people by name case-insensitively, as Jellyfin 12.2 does, so a
  lowercase name such as `de Niro` lists among the D names instead of after every
  capitalised one (#60)
- An episode, season or music track stored without a sort name, which the startup repair
  fills, now sorts by its numbers, as Jellyfin's own sort names do. Before, the repair gave
  it an alphabetical key, which put episodes out of order and could break "next episode"
- Collections and playlists take their genres and studios from their members, as in
  Jellyfin. A collection also takes its earliest member's date and year, and a playlist's
  runtime is the sum of its members'. Before, they took only a rating, so they had no
  genres, studios, year or runtime and never matched a genre or studio filter. Locked
  items and locked fields keep their values. A collection also keeps a current list of
  the libraries its members are in, which decides who can see it
- Adding, removing or renaming a library, or adding or removing one of its folders, can
  fail part-way, for example on a full disk. When it does, the request still fails, but
  the server now also refreshes its folder watch and runs the scan the request asked for,
  as Jellyfin does. Before, both were skipped until the next scan
- A library's folder shortcuts are written as Jellyfin writes them. They are named after
  the folder without its extension (`Films.HD` gives `Films.mblink`), and a folder inside
  the server's data directory is stored as `%AppDataPath%/…`, so it keeps working if the
  data directory moves
- During a transcode, PGS and VobSub subtitles a client can draw itself are delivered as
  separate subtitle files, as in Jellyfin 12, instead of being burned into the video. The
  "Allow subtitle extraction on the fly" setting now applies: with it off, other embedded
  subtitles are burned in during a transcode. Playback negotiation also asks the installed
  ffmpeg which audio encoders it has, instead of assuming a fixed list
- The Playlists folder is created with Jellyfin's dates: created and modified from its
  directory, and saved now. Before, it had no modified or saved date. A one-off startup
  repair fills the dates missing from the Playlists folder and the Collections library
  that earlier versions created, and leaves every other library alone
- The configuration files written at startup (`system.json` on a first or adopting start,
  when the port is authorized and by the setup-completed repair, and the named documents
  seeded from Jellyfin's XML) are replaced atomically and readable by the server's user only
  (`0600`), like every other configuration save. A server stopped mid-write could leave a
  truncated `system.json` that kept the next start from booting
- A Jellyfin configuration file that cannot be imported no longer stops the ones after it. A
  corrupt `encoding.xml` or `branding.xml` kept `network.xml` (the remote-IP filter, local
  subnets and known proxies), `metadata.xml`, `xbmcmetadata.xml` and `livetv.xml` from being
  imported on every start, and only the broken file was named. Each file is now imported on its
  own, and each one that fails is logged with its path
- An `encoding.json` the server cannot read (after an ownership change, say) is logged once as
  a warning while transcodes run, as the transcode planner already did, instead of an error
  every five seconds for each running transcode
- `GET /System/Configuration/{key}` reads through the configuration manager like the rest of
  the server. A saved document that does not parse is logged once per change of the file
  instead of on every request. One that exists but cannot be read answers 500 instead of
  reading as the defaults, and so does saving the encoding settings over it, so a save cannot
  replace a document nobody could see (Jellyfin serves the defaults when it lacks permission)
- Two administrators demoting or disabling each other at the same moment can no longer leave
  the server without an administrator, or without any enabled user. The checks that
  `POST /Users/{userId}/Policy` makes now run inside the save itself, as deleting a user's
  already did, so the second save sees the first. Jellyfin has the same race. A user's tokens
  are revoked once the save that disables them has gone through, so a refused save signs
  nobody out
- The server opens its HTTP port as soon as it starts, before it adopts or migrates the
  database (after a scheduled backup restore), and answers while it does, as Jellyfin's
  startup server does:
  `/health/live` 200, `/health/ready` 503, `/health` `Degraded`, every other request 503 with
  `Retry-After` and a "starting" page. Before, the port stayed closed until boot finished,
  so a long adoption could outlast the chart's startup probe and be restarted in a loop.
  Boot stages are now logged at `info`
- A recursive `/Items` request sorted by `AiredEpisodeOrder` orders by sort name, as Jellyfin
  does, and a folder's children sorted by it use the aired-episode comparer
- `/Items?ids=` and `/Trailers?ids=` without a `sortBy` return the items in the order their
  ids were requested, as Jellyfin does, instead of in sort-name order
- A Live TV stream that a client's start and progress reports never named is closed by its stop
  report or by the teardown of its abandoned transcode, as Jellyfin does, instead of holding the
  tuner until a restart
- The playlists folder is made only at startup, as Jellyfin does, instead of being looked for
  again, with a rule of its own, on every `GET /Library/MediaFolders` and Folders view request
- After the data directory moves, startup moves the root folder and the playlists folder to
  it, as Jellyfin's `CreateRootFolder` does, and creating a playlist only looks the playlists
  folder up. Before, `POST /Playlists` missed the folder at its old path, tried to create it
  again under the same id and failed with a 500
- A DVR timer whose row cannot be read when it fires is retried every minute, up to ten
  times, and logged, instead of the recording being missed with no trace. A stored timer or
  series timer that no longer parses is logged when it is first read and when it misses its
  recording, instead of vanishing silently
- An M3U playlist, HDHomeRun or Schedules Direct request, a File Transformation callback or
  the relay of a remote static stream gives up after 100 seconds, as Jellyfin's do, when the
  other end accepts the connection and never answers. Before, it waited for ever, stalling
  the guide refresh, tuner discovery or the client's request. The M3U, HDHomeRun and relay
  limits stop at the response headers, so a long body or stream is never cut off
- A scheduled task that panics is recorded as failed, with an alert on the dashboard, and can
  run again, as in Jellyfin. Before, it stayed "Running" until someone pressed Stop, the
  server restarted or a trigger's time limit ran out, and its triggers and the Run button did
  nothing
- A request whose handler panics is answered `500` "Error processing request." and logged,
  as in Jellyfin. Before, its connection was dropped, over HTTP/2 with every other request on it
- A season with more than 20 episodes that Jellyfin's aired-order rule cannot fully order (an
  undated episode among dated ones without episode numbers, or an unnumbered one beside a
  special placed before a later episode) is listed again: `GET /Shows/{id}/Episodes`, the
  season's `GET /Items` and the scan's season premiere-date pass sort it the way Jellyfin's
  stable sort does, instead of answering `500` or failing the scan
- An M3U playlist is read line by line as it downloads and an HDHomeRun device's JSON is parsed
  as it arrives, as Jellyfin does, instead of buffering the whole response: a large playlist is
  no longer held whole in memory, a wrong playlist URL that serves a video stream keeps memory
  flat (the guide refresh still waits on it, as Jellyfin's does), and a device answering with
  something that is not JSON fails at once. A playlist whose first line follows a UTF-8
  byte-order mark, or whose lines end with a lone carriage return, no longer loses channels
- An XMLTV guide is downloaded into its cache file and read from it as a stream, gunzipped
  on the way, as Jellyfin does, instead of being held whole in memory for the download, the
  cache and both reads of a guide refresh: a large IPTV guide no longer makes every refresh
  peak at its size, and a source that never ends no longer grows memory until the download
  times out. A download that fails or is abandoned leaves no temporary file behind
- A guide refresh re-arms a live recording timer that has no arm, as Jellyfin's refresh of
  every timer does, not only one the refreshed guide changed, so a timer whose arm was lost
  records again; a timer fired while its capture is still starting no longer risks recording
  the showing twice
- Deleting a tuner host queues a guide refresh, as Jellyfin 12 does, so its channels and
  programmes leave item queries, search and genre lists at once instead of up to a day later
- A new playlist is announced to the sessions that may see it as added, and renaming it,
  making it public or private, changing who it is shared with, moving an entry or handing it
  to a share when its owner is deleted announces it as changed and moves its `Etag`, as in
  Jellyfin. Before, a new playlist was announced as changed, so another open jellyfin-web
  session showed it only after a reload, and the other edits were not announced at all
- Listing the libraries no longer fails when it cannot write a library's database row (a busy,
  full or read-only database): the failure is logged as a warning and every library is still
  listed, where `GET /Library/VirtualFolders`, the home screen's views and the scheduled tasks
  that look libraries up all failed until a later listing succeeded
- Image, user image, splashscreen, subtitle and lyric uploads take bodies up to 30,000,000
  bytes, Kestrel's default that Jellyfin keeps: an image over about 1.5 MB (2 MiB once
  base64-encoded) was refused with a 413. A body past a route's limit is a 413 everywhere,
  where a JSON body over it was a 400 claiming the body was missing
- A posted device profile the stream builder cannot evaluate is answered as Jellyfin answers
  it, a 400 for a condition on a property the stream has none of and a 500 for a comparison
  its value has none of, instead of a panic. A `VideoTimestamp` condition's value is read as
  Jellyfin reads it, so an unknown one is a 400 rather than a condition never met
- A resized image is written beside its cache path and renamed into place, so a request that
  races its encode, or follows an encode that failed part-way, never serves (and pins for a
  year, under the immutable cache header) a truncated image
- The Merge Versions plugin's bulk merges and splits run one at a time, whether a route or a
  daily task starts them: a merge and a split started together interleaved their version-link
  writes
- Renaming a library to its own name, or changing only its case while item ids ignore case,
  keeps the library's database row. It was deleted and created again, which reset the
  library's date added, dropped its images and left its items without the library above
  them until the next scan
- A transcode whose first segment takes longer than the idle timeout (60 s for HLS, 10 s
  progressive), such as a cold 4K software encode or a slow tuner, is no longer killed by the
  idle reaper while it starts. A transcode killed while it starts, by a stop request say, ends
  its request at once instead of after three minutes
- Playing a `.strm` shortcut, or an item with no main stream, refreshes it as an item refresh
  queued with the library scans, as in Jellyfin. It no longer runs a scan of its own beside a
  library scan, and a shortcut whose file is gone is no longer deleted while it is played
- Two refreshes of one item's trickplay at once (a scan's and the daily task's, say) generate
  its tiles once, and one no longer deletes a tile folder the other has just moved in
- When the kernel's inotify queue overflows during a bulk import, the watched libraries are
  rescanned, and a warning names the `fs.inotify.max_queued_events` sysctl to raise. The
  missed changes used to stay out of the library, silently, until the next scheduled scan
- A library change while the watches are still being set up at boot no longer leaves that
  setup watching the libraries it had read, a removed one included, until the next restart.
  An in-process restart stops the setup
- Closing a stream from a legacy (pre-vchannel) HDHomeRun releases its tuner. Every close left
  one tuner locked, and after as many closes as the device has tuners every open failed with
  "No tuners available" until the device dropped the locks
- An attachment or font extraction cancelled by a disconnecting client stops its ffmpeg, and
  deletes the half-written files the cache would otherwise have served. A batch font dump,
  whose files the media names, leaves what it wrote, as in Jellyfin
- The README said a fresh install logs a generated admin password. It seeds the admin with
  no password, as Jellyfin does: the setup wizard at `/web` sets it, or
  `FERROFIN_ADMIN_PASSWORD` seeds one
- WASM plugins: a web transform whose `path-pattern` is not a valid regex, such as the glob
  `*.chunk.js` the plugin interface gave as an example, is refused at load with a warning; it
  matched nothing, silently. The interface now says what a pattern is: an exact path under the
  web root, else a regex matched anywhere in the path
- Plugins: overlapping installs, updates and uninstalls can no longer lose a recorded entry
  from the plugin state file (`plugins/state.json`) or tear it, and a plugin update racing a
  manual install of the same plugin can no longer tear the staged `.wasm`. Every save is made
  under the state lock and through its own synced temporary file, where three of them wrote an
  older copy of the state outside the lock and every writer of a path shared one `.tmp` file.
  These files (the state, plugin settings and staged `.wasm`) are now written owner-only
  (`0600`). A state file that cannot be read or parsed is kept as `state.json.corrupt` and
  logged at error level instead of being silently replaced, along with the enabled switches
  and the record of installed versions it held
- File Transformation: a callback endpoint that fails (an error status, a timeout, a refused
  connection) logs one warning, then logs at debug level until it works again. It logged two
  warnings on every matching `/web` request for as long as it stayed down
- The config file is read as TOML only: a `--config`/`FERROFIN_CONFIG_FILE` path named
  `.json`, `.json5`, `.yaml`, `.yml`, `.ini` or `.ron` is refused instead of parsed in that format
  (see [docs/UPGRADING.md](docs/UPGRADING.md)), and the server no longer builds those
  parsers
- OTLP trace export works with an `https://` collector: the exporter now has TLS (aws-lc) and
  checks the collector against the system trust store. It refused an https endpoint before, and
  that went unlogged, like every other exporter outcome: the server logged them before its
  log output was set up. The startup log now says whether traces are exported, or why not
- `{data_dir}/config.toml` is read for a data directory named by `--data-dir` or the default
  location, not only by `FERROFIN_DATA_DIR`; with both, the flag's directory names the file.
  The startup log names the file (`config_file`, `config_file_found`), and a `data_dir` key in
  a file found in the data directory is ignored with a warning (see
  [docs/UPGRADING.md](docs/UPGRADING.md))
- Dashboard → Restart loads `config.toml` again, so its changes apply without restarting the
  process; the data, configuration and cache directories, `log_level`, `log_format` and
  `metrics_scan_duration_buckets` keep their values until the process restarts (with a warning
  naming them), and a file that no longer loads is logged and leaves the previous
  configuration running. A `config.toml` parse error names the line and column but no longer
  quotes the line, which may hold a password
- A `config.toml` key the server ignores, a misspelling or a variable's stem where the key is
  spelled otherwise (`tvdb_pin` for `tvdb_subscriber_pin`), is a startup warning naming it and
  the key to use, and so is a number or on/off variable that does not parse
  (`FERROFIN_PORT=80g`); both used to be dropped without a word. `docs/CONFIG.md` lists the
  seven keys that are not their variable's stem, and documents `FERROFIN_DISABLE_EXTENSIONS`
  and `FERROFIN_MAX_PLUGIN_DOWNLOAD_MB`. The startup log's `admin_password_set` no longer
  reports a blank password as set
- The metadata-provider request limits and the stdout log format are `config.toml` settings
  too (`provider_timeout_seconds`, `provider_max_wait_seconds`, `log_format`), beside
  `FERROFIN_PROVIDER_TIMEOUT_SECONDS`, `FERROFIN_PROVIDER_MAX_WAIT_SECONDS` and
  `FERROFIN_LOG_FORMAT`, which keep working as before. The provider limits now apply on
  Dashboard → Restart; a limit outside its range, or a log format other than `json` or `text`,
  logs a warning. The `OTEL_*` variables are documented as environment only
- `FERROFIN_LIBRARY_ROOTS` (`library_roots` in `config.toml`) does what it is documented to do:
  a fresh install starts with one library per listed folder, named after it, and scans them.
  Nothing read the setting before, so a headless install that set it started with no libraries.
  A later boot, or an adopted Jellyfin database, adds none

## [1.0.0] - 2026-09-05

### CI/CD
- Harden the workflows — SHA pins, least privilege, zizmor gate, CODEOWNERS

### Documentation
- Observability section — JSON logs, Prometheus metrics, OTLP traces
- Point plugin authors at ferrofin-plugin-template and its manifest
- Examples/wasm-hello is the WIT conformance fixture, the template is for authors
- Reset to the 1.0.0 baseline; drop the pre-release fetcher note
- Remove references to private plans, local skills and personal paths
- About update

### Miscellaneous
- Single vendored spec copy, drop the .port allowlist
- Small readme.md update

### Testing
- Stop racing statvfs against parallel tests in the fs-probe check

## [0.42.7] - 2026-09-05

### CI/CD
- Move to the Node 24 majors of the artifact and docker actions

## [0.42.6] - 2026-09-05

### Bug Fixes
- Bake the release tag into the binary's build version

### CI/CD
- Assert the image ships jellyfin-ffmpeg

## [0.42.5] - 2026-09-04

### Bug Fixes
- Compile on macOS and Windows targets

### CI/CD
- Check /health/live for the build tag, and boot the pushed image

## [0.42.4] - 2026-09-04

### CI/CD
- Build and attach server binaries to the GitHub Release
- Use actions/checkout@v5 (Node 20 runtime deprecation warning)
- Header says GitHub is canonical, not GitLab
- Grant actions:read so the green-CI gate can list runs

### Documentation
- Documentation updates

### Testing
- Cover the Database query helpers, backup and integrity paths

### Report
- Report the run-to-run range, stop ruling on it

## [0.42.3] - 2026-09-04

### Counts
- The whole-server played arm counts leaves, not user-data rows

### Report
- Make the headline tiles fit the window

## [0.42.2] - 2026-09-03

### Bench
- A third load level, and the guards a third level needs
- Name a run for the code it measured

### Counts
- Pin the played roll-up to drive from the page's parents

## [0.42.1] - 2026-09-03

### Bench
- A report you can read — markers in the cells, reasons in notes
- Report the precision the measurement actually has

### Latest
- Port v12's music branch — the newest albums, not the albums of the newest tracks

## [0.42.0] - 2026-09-03

### Bug Fixes
- Every measured endpoint counts in the headline
- A missing leg marks the run, it does not delete it
- Repair the render and stop hiding measured endpoints
- HEAD moving mid-run is not a stale build
- Repoint the Live TV foreign keys 0009 left dangling on adopted databases
- Name both causes when testdata bring-up cannot reach a server
- Footprint metrics measure real numbers — restart-based cold start, 100ms host-cgroup RSS sampling
- Scope to the user's libraries, port v12 DetermineNextEpisode — 1.29 s → 12 ms

### Features
- Keep both servers' responses beside every parity verdict
- Refuse to run if real media is writable from any suite container
- Pin a real Jellyfin snapshot as the suite's test data
- Both servers run on seeded copies of the pinned snapshot
- Pinned backup + a read-only streams stage — the two media rules
- Three-percentile speedups; verification depth leaves the perf headline

### Miscellaneous
- Regenerated parity results and the v0.40.0-82 run record
- Record the v0.41.0-3 run (130 measured, 1.94x, 1 leg missing)
- Record the v0.41.0-6 run (136 measured, 1.795x, no missing legs)
- Keep the code-graph index out of the docker build context
- First snapshot-corpus record — recalibrated rates, run 240db12, acked shape baseline

### Api
- The parity record lives beside the route table
- GET /Items and /UserItems/Resume take v12's DtoOptions and query inputs

### Bench
- Designed test data — generator, Jellyfin seeding, build
- The comparison run, its instruments and the report
- Report interference as p95 / max, not max alone
- The comparison viewer — report.py --serve
- Benchmark core Ferrofin — every plugin disabled, no WASM
- Shape records items returned and TotalRecordCount separately

### Dto
- V12 parity for the small DTO fields (P1.1–P1.6, P1.9)
- Port v12's inherited parent images and Series.Status

## [0.41.0] - 2026-09-01

### Bug Fixes
- Align the lab data dirs and close the folder-DTO divergences
- Pick the C# provider per kind, and 404 a seed that is not there
- Order the flat providers Random, and stop calling a property probe deep-verified
- Port the probe DTO, by-name and library-options divergences batch A3 found
- Port image format negotiation, the refresh fetcher gate, and the music/search sort keys batch B2 found
- Port the ?format= query binding arm and stop the asset layer borrowing the body-diff headline
- Make every ledger row declare how it was verified, and stop defaulting into the headline
- Restore merge.py's body-diff honesty gate, which the stamping commit dropped
- Stop the deep-verified headline being borrowed by a nested empty envelope, a dashboard, or an unenforced rule
- Save where Jellyfin saves, parse what Jellyfin parses
- Port the real LrcParser decoder, and gate the writes on LyricManagement
- Make tuner and listings-provider administration do the work
- Queue the guide refresh instead of blocking on it, and stop a side-path note standing in for a verdict
- Port the AggregateFolder root and its virtual-children concat
- Port the /Items user-root branch, and make array order diffable
- One repository store, a real package catalogue, no orphan views
- Bind assemblyGuid the way ASP.NET binds a Guid?
- Bind a blank assemblyGuid the way ASP.NET does
- Rename the ferrofin volume after its mount point moved
- Port the Years/InstantMix/dashboard-page behaviours the "out-of-scope" notes were hiding
- Close the cross-user read, and port the plugin surface the "no shared id" notes were hiding
- A non-admin could act as any other user through ?userId=
- Close the last GetUserId site, and bind an empty guid the way ASP.NET does
- Four "host-specific" rows were false labels hiding real bugs
- The task Id is portable, and the ffmpeg logs belong in the log dir
- The probe DTO tracks upstream master's nullability again
- Correct the record M1 wrote, not just the code
- Parent the playlists folder, and port the id/image lineups to 10.11.8
- Teach every stored-kind switch the real PlaylistsFolder name
- Derive channel/programme ids the way Jellyfin does, and bound the guide
- Port the whole UserPermissionRequirement policy, not just its handler
- Gate remote search on the library's downloaders, and stop typing artwork from the URL
- Make the Identify Apply row say what it actually measured
- A bare empty list is untested, not empty-corpus
- The server mints a series timer's id, and the timer it schedules
- A hand-cancelled showing stays cancelled, and cancelling a series takes its completed children
- A tuner type is a lookup, a scan is a filter, and a source is the user's
- A channel is a real item, a series timer keeps its name, and a refused body is a 400
- A guide refresh owns a channel's lineup fields, not its item
- An airing is a real item, and a keyless row is not a group
- A keyless by-name row is item loss, not a grouping detail
- A channel no provider backs is 400, and four labels were wrong
- The Startup guard row asserts the guard, not a payload identity
- Repair the series-timer fan-out probe and stop three reds rendering as settled
- Port CreateItemByName, cumulative runtime ticks and the instant-mix dispatch
- Resolve instant-mix genre names through CreateItemByName, and restrict by-name counts to their ItemValue type
- Resolve LibraryOptions by parsed Uuid, not by id spelling
- Write JPEG the way Skia does, and stop advertising ranges on trickplay tiles
- Invalidate the image cache after the JPEG chroma change
- Port the RemoteSearch "Identify" cascade faithfully
- Pin a padded provider id the way int.Parse does
- Route POST /Library/Refresh through the task registry
- Port the group-state SessionJoined/SessionLeaving hooks; add a two-server push differential
- Port WaitingGroupState's third ResumePlaying arm; make the push probe report what it drops
- Port MusicArtistResolver; RecursiveItemCount; the remote-search fetcher gate
- Port the remote-search provider ordering; loose artist audio
- Port the per-state playback arms; drive them over the push differential
- Port the WaitingGroupState fallback; widen the push probe to every state
- A fresh play queue has nothing playing, not item 0
- Port the slug branch, per-CleanName counts and the Person row columns
- Port the PresentationUniqueKey collapse; guard ForcedSortName
- A missing traceId must diff red, and drop the dead Year guard
- Close the paths that let a wrong number look like a measurement
- A side-path note never settles the row it hangs off
- Filter LibraryChanged by what the user may actually see
- Derive LibraryChanged CollectionFolders the way GetTopParentIds does
- Stop delete_reports_a_failed_unlink failing when CI runs as root

### Documentation
- Record that Live TV channels 404 on GET /Items/{itemId}
- Make the body-binder claim carry its measurement
- Name the two unported IsMetadataFetcherEnabled arms

### Features
- A tuner is a kind, and HDHomeRun is one of them
- Port LibraryChangedNotifier so item writes announce themselves

### Miscellaneous
- Record the v0.40.0 parity+perf run

### Security
- Join the wrapped warning string in ensure_startup_wizard_integrity

### Testing
- Cover the user-root branch's IsVisible arm
- Probe the external-change webhooks; stop dropping ProviderIds on an item edit
- Regenerate the ledger — push layer verifies 20/20

## [0.40.0] - 2026-08-29

### Bug Fixes
- Write user data under the keys Jellyfin actually reads
- Key the Peoples cover index on a collation, not on LOWER()
- Anchor an interval on the last run, not on process start
- Serve NetworkConfiguration under the contract's field names
- Resolve jellyfin's virtual libraries, and find by-name rows by name and kind
- Confine an unscoped query to the user's libraries, and give created items a container
- Read an adopted Jellyfin database the way Jellyfin reads it
- Stop folding eth and thorn, and keep the keys we cannot rebuild
- Confine the counts and the by-name tabs to the user's libraries too
- Confine the filter facets and /Years to the user's libraries
- Resolve a UserView the way GetTopParentIdsForQuery does
- Make the perf gate say what is actually wrong
- Give a provisioned container the shape Jellyfin will recognise
- A grouping view stands for the libraries grouped into it
- Resume surfaces the version that was played, not the primary
- Reduce memory usage of dev builds

### Features
- Import jellyfin's config xml when adopting a data directory
- Enforce the network policy instead of only storing it
- Import network.xml when adopting a Jellyfin data directory
- Import Jellyfin's tuners and guide on adoption
- Resolve the real client address behind a known proxy

### Miscellaneous
- Record the ferrofin-core -> ferrofin-networking edge in the lockfile

### Testing
- Cover KnownProxies and the forwarded-header walk

## [0.39.1] - 2026-08-27

### Bug Fixes
- Scope Merge Versions' movie merging to one library

## [0.39.0] - 2026-08-27

### Bug Fixes
- Stop the benchmark measuring Jellyfin's startup stub

### Documentation
- Scope hardware transcoding to NVENC, VAAPI and QSV

### Features
- Probe the full ffmpeg capability surface
- Build the hardware device graphs and pick hardware encoders
- Choose the hardware decoder and assemble the input line
- Port the tonemapping paths and the shared filter fragments
- Port the software filter chain and the filtergraph assembly
- Port the NVENC/CUDA filter chain
- Drive NVENC from the ported hardware matrix
- Probe the VAAPI render node for its driver and Vulkan interop
- Port the Intel iHD and limited VAAPI filter chains
- Complete VAAPI -- AMD Vulkan chain, low-power params, wiring
- Port the QSV gate and the Linux QSV filter chain
- Complete QSV -- D3D11 chain, bitrate arms, wiring
- Strip Dolby Vision and HDR10+ metadata on a stream copy
- Report a starting transcode, and what hardware it runs on
- Port the accelerated trickplay path, and fix -vsync on ffmpeg 8+
- Run trickplay extraction on the GPU

### Miscellaneous
- Regenerate the parity ledger and record the latest benchmark runs

### Performance
- Drop the redundant DISTINCT from the by-name item count
- Skip the browse row-count when the page already proves it

### Testing
- Stop the VAAPI device goldens depending on the test machine's GPU
- The real-Jellyfin drop-in adoption harness

## [0.38.0] - 2026-08-24

### Testing
- Stop the restart tests failing on a busy machine

## [0.37.1] - 2026-08-23

### Bug Fixes
- Make scrubbing previews reachable by clients; honour the per-library gate
- Stopping a scheduled task kills its ffmpeg child
- Persist SortName on every write, and port CreateSortName faithfully
- Gate POST /System/Restart behind local-access-or-elevation

### Performance
- Default the scan probe window to 8
- Stop asking folders and by-name items for media sources
- Take the playback-metrics writes off the request path

## [0.37.0] - 2026-08-23

### Bug Fixes
- Derive Person ids from the case-normalized by-name key

### Documentation
- Retire the "deferred" classifications after the no-deferral sweep

### Features
- Port GetLocalizedString and re-stamp MediaStream.Localized* on read
- Index external subtitle/audio sidecars at scan time

### Performance
- Stop memsetting 128 KiB per pushed WebSocket message
- Stop re-decoding unchanged images on every rescan

## [0.36.0] - 2026-08-23

### Bug Fixes
- GroupingOptions lists only grouping-eligible libraries
- Subtitle writers match Jellyfin byte for byte
- Media sources carry their attachments and the N-form id
- Write guids and dates the way Jellyfin's JSON converters do
- Accept the contract's videoBitRate/audioBitRate spellings on HLS routes
- Port GetLatestItems — one query, grouped by index container
- Persist the whole user policy; classify guide programmes
- Stamp DateCreated/DateModified the way Jellyfin's resolver does
- The liked threshold is 6.5, as UserItemData.MinLikeValue says
- Hand the trickplay manager to the HLS chain
- Match the segment container case-insensitively
- Release the tuner when a transcode job dies
- Release a tuner once per viewer, and only when the client is gone
- Keep LastExecutionResult, and never lose a startup run
- Build by-name DtoOptions from the request, as Jellyfin does
- Give /Similar Jellyfin's phase-2 filter, seed guard and artist exclusion

### Features
- Port HlsCodecStringHelpers — the RFC 6381 CODECS strings
- Port the tvshows/music grouped-threshold latest query
- Port DynamicHlsHelper — the real master.m3u8
- Project channels through the DTO service, as Jellyfin does
- Project programmes through the DTO service, as Jellyfin does
- The "Refresh Guide" scheduled task
- Serve the live.m3u8 ffmpeg writes, as Jellyfin does
- The real live-stream flow — open a tuner once, share it, close it
- The DVR — timers that fire, recordings that play while recording
- Transcode the live stream a client opened, not the channel again
- The "Update Plugins" scheduled task
- The "Refresh Channels" scheduled task
- The core "Media Segment Scan" task, always registered
- Port the remaining Identify / remote-image / lyric paths
- Proxy the Schedules Direct country list as Jellyfin does
- Materialize the UserRootFolder, Year items and by-name collages

### Performance
- Index the SortName ordering, and give it Jellyfin's tiebreaker

### Refactor
- One CreateSortName port for the scanner and the guide

### Testing
- Pin the canonical-id close and the sort-key re-derivation

### Parity
- Bring the misclassified not-testable ops under real test
- Verify the host-side effects — password reset, backup, restart
- Terminal phase — restore, restart, shutdown observed live
- Fixtures and provisioning for music, subtitles, trickplay, Live TV
- Verify subtitle delete and media attachments
- Stream-signature layer for direct play, HLS, subtitles, trickplay
- Live TV journey — live streams, timers, in-progress recordings
- Opt-in remote-subtitle journey through OpenSubtitles
- Regenerate the ledger with the new layers live

### Style
- Rustfmt the live-stream planner test

## [0.35.0] - 2026-08-23

### Bug Fixes
- Project media folders without a user, as Jellyfin does
- Honour the client's fields on playlist items
- Stop exposing live access tokens in device listings
- Never gate local sidecar artwork on the ImageFetchers list
- Serve UserViews with Jellyfin's full default field set

### CI/CD
- Run the coverage gate before the Rust toolchain

### Features
- Shape honesty gate v2 — self-baseline excludes, Jellyfin diff publishes

### Performance
- Event-driven segment waits; gate the fMP4 init on segment 0

### Testing
- Assert the local-artwork regression via the repository seam

## [0.34.0] - 2026-08-22

### Bug Fixes
- Stop re-provisioning the Playlists folder on every read

### CI/CD
- Enforce the benchmark-coverage gate

### Features
- Add the extension kill-switch the benchmark assumed existed

### Performance
- Compress at ASP.NET's level instead of the codec's default
- Cut /Shows/NextUp CPU 6.5x — it was CPU-bound, not pool-bound

## [0.33.3] - 2026-08-22

### Bug Fixes
- Correct provider parity defects found in review
- One library read per similar-items request, and keep remotes out of recommendations
- Correct EXIF, NFO and EPUB edge cases found in review
- Close the remaining provider-parity gaps found in review
- Resolve remote similar-item batches the way C# does
- Persist the ids an NFO pins, and cap archive reads on bytes read
- Complete the photo DTO and keep EXIF reads off async workers
- Match upstream on book, playlist and OMDb lookup details
- Resolve items by their recorded ids, and correct three misported details
- Keep the presentation key in step with a merged version group
- Hoist test const above statements for clippy

### Documentation
- State plainly that the container-XML and playlist-file ports are unwired

### Miscellaneous
- Cleanup

### Performance
- Scope the episode text read to the items the scan planned
- Cache OMDb season listings, and finish the EXIF port
- Only compress responses larger than one TCP segment

## [0.33.2] - 2026-08-21

### Bug Fixes
- Default fsGroup 1000 so volume ownership self-heals on mount
- Gate the rest of the RequiresElevation surface

## [0.33.1] - 2026-08-21

### Bug Fixes
- Take backup work off the runtime, gate it, and serialize it

### Performance
- Stop reading the item row to serve an image that exists
- Probe fpcalc concurrently instead of on the critical path
- Resolve a page's cast without materializing 72-column rows
- Keep exporter-only span fields out of the no-export path
- Read the locked-item set once per scan, not once per item

## [0.33.0] - 2026-08-20

### Bug Fixes
- Match C# ordering and null semantics in similar items
- Translate a cast play request before pushing it
- Enforce the SyncPlay access policy on every route
- Gate SyncPlay groups on each member's library access
- Expand linked-child containers, and drop the SyncPlay list N+1
- Halve the reads in play translation, dedup, log denials
- Never re-derive an item id from the row in play translation
- Enforce Jellyfin's RequiresElevation policy

### Documentation
- Record how to profile the push paths, and what fails here

### Testing
- Make the push surface a first-class suite stage

### Style
- Formatting after the main merge

## [0.32.0] - 2026-08-20

### Bug Fixes
- Give episodes their title and synopsis from TMDB
- An episode's Cast & Crew comes back from TMDB
- Let a completed credits fetch clear a stale cast
- Make a failed frame extraction say why
- A failed credits request is not an empty cast
- Close the same credits hazard on the TVDB arm
- Gate episode re-fetches on the stored row, not the planned one
- Let TheTVDB report an episode miss, so TMDB gets a turn
- Give up when every chapter extraction is failing
- Only a real extraction resets the chapter failure streak
- An image already on disk is not proof extraction works
- A video that extracted before failing is both things
- Parent photos to their own directory's album
- Date folder-named audiobooks, correct books scan comments
- Skip cue sheets and emit an empty book SeriesName
- Resolve XML entities in XMLTV attribute values
- Bound two collections that grew for the life of the process
- Close three read-then-write races on the user and session paths

### Features
- External id descriptors and the item Links row
- Full OMDb metadata, image and search provider
- TMDB box-set metadata, artwork and identify
- Scan photos and read their embedded EXIF
- Album.nfo and artist.nfo readers and savers
- Remote similarity providers (TMDB similar, ListenBrainz)
- Scan books and read their embedded comic/EPUB metadata
- Collection/playlist XML, playlist files, MusicBrainz depth
- Scan books libraries into Book and AudioBook items

### Miscellaneous
- Drop dead cargo features and correct provider docs

### Performance
- Seek folder leaf counts from the ancestor closure

### Revert
- Drop the chapter failure-streak guard

## [0.31.0] - 2026-08-20

### Bug Fixes
- Actually apply the program query filters in the manager
- Stop degrading unparseable ids to the nil GUID
- Rank search results in SQL, before the LIMIT
- Scope socket unregistration to the socket that closed
- End a session when its last socket closes

### Features
- Compress responses, as Jellyfin does

### Performance
- Let jemalloc return memory while the server is idle
- Pipeline ffprobe during the library scan
- Trim the scheduled-task scheduler's per-tick work
- Skip the backtracking engine when a pattern provably cannot match

## [0.30.3] - 2026-08-20

### Bug Fixes
- Close five races that real parallelism made reachable
- Restore transcode keep-alives, honour SegmentLength, bound the log read

### Documentation
- Record what the fast loop cannot measure

### Performance
- Stop ORDER BY RANDOM() serializing on SQLite's global PRNG mutex
- Cut cold start 3.1x by overlapping the external probes
- Skip the user-data push read when nothing can receive it
- Return the server configuration by Arc instead of by deep clone

## [0.30.2] - 2026-08-20

### Bug Fixes
- Benchmark suite updates
- Stop holding the session lock across WebSocket sends
- Authenticate the hls1 segment routes, matching Jellyfin
- Scope /Items/Filters2 by parent regardless of `recursive`
- Stop slicing branding CSS with an index from its lowercased copy
- Log the config read failures that rendered as "empty"
- Make the two culture lists agree on their ordering
- Keep an unset display preference null instead of ""
- Apply the Live TV program query filters
- Restore the SQL-boundary ratchet

### Miscellaneous
- Refresh ledger from a full sweep — 191/412 deep-verified
- Drop the last of the old project name

### Performance
- Move prefetched relation rows into DTOs instead of cloning
- Move an item's stream rows into the DTO at their last read
- Move a stream row's text fields into the DTO instead of cloning
- Collapse the NextUp episode projection into one query
- Collapse the playlist-items read path
- Push LIMIT/OFFSET into the /Persons query
- Stop re-parsing and re-cloning on the authenticated request path
- Bind borrowed names in the by-name count queries
- Build the localization tables once
- Stop recomputing DTO lookup keys per name per item
- Stop buying the /Persons total with a second full pass
- Stop blocking a tokio worker per image row
- Remove SQLite's two global lock bottlenecks
- Request SQLite's maximum mmap ceiling, and pin why it needs no tuning
- Batch the /Devices listing out of its N+1
- Batch the per-item user-data reads behind /Items/Latest
- Force the join order on the suggestions aggregates

### Refactor
- Store the server configuration behind an Arc
- Rename the Ferrofin-owned schema objects off the old project name

### Testing
- Cover the legacy-authorization gate
- Pin the person-name key convention on both sides
- Add suite/micro — a ~10s measurement loop

## [0.30.1] - 2026-08-18

### Bug Fixes
- Address review findings — empty-page total, cycle guard, chunking
- Bump library_manager SQL boundary ceiling for cycle test

### Miscellaneous
- Update performance gate baseline

### Performance
- Jemalloc + batch queries for saturation-family endpoints

## [0.30.0] - 2026-08-17

### Bug Fixes
- An episode's Cast & Crew is the episode's, not the series'
- An episode's Cast & Crew is the episode's, not the series'
- Benchmark v2 — address review round 1 (blockers, ratio floor, pooling)
- Benchmark v2 — address review round 2 (viewer subline, warmup call floor, ratio-floor tests)
- Benchmark v2 — address review round 3 (vacuous test 11, ratio accounting totality)
- Honor FERROFIN/JELLYFIN_HOST_PORT in the perf legs
- Per-checkout compile-cache scope (CACHE_SCOPE) — shared cargo cache mounts cross-poisoned checkouts
- Idempotent provisioning + ctx files exempt from the raw wipe
- Scope the bench image tag like the compile cache
- Cap calibrated rates at BENCH_RATE_MAX (2000/s)
- Login storm runs LAST + failure taxonomy on partial rows
- Graceful stop for cold-leg restarts — compose restart's 10s grace SIGKILLed Jellyfin into an unbootable DB
- Cold probes wait out Jellyfin's post-restart 503 window, record it as ready_wait_ms
- 0.5 review round 1 — sidecar escape, load-time name guard, caps become settings
- 0.5 review round 2 — close the five low residuals

### Build
- Profiling profile — release speed with full debug info

### Documentation
- Profiling instructions — from benchmark row to root cause
- Perf_event_mlock_kb sysctl for samply on many-threaded servers
- Make the fetcher-enforcement upgrade note survive changelog regen

### Features
- Benchmark v2 phase A+B — fail-loud manifest + verified binary identity
- Benchmark v2 phases I+G — single-language Python suite on vegeta, open-loop comparison legs
- Benchmark v2 phase H — warm/cold protocol, cold as a first-class metric
- Benchmark v2 phase E — core-vs-extension ownership, machine-readable
- Benchmark v2 phase D — the noise floor is a first-class tie
- Benchmark v2 phase C — publishable records are N-run distributions
- Benchmark v2 phase F+G2 — fairness polish + the saturation knee
- Scripted rate calibration — suite/run.sh calibrate
- G4 — operator-tunable per-plugin state cap
- G2 — the scoped write family (WIT 0.5.0)
- G3 — embedded subtitle extraction
- G1.1 — richer metadata-result (supplement-only, entity-backed)
- G1.2 — named providers surface in library options
- Remote-images (G1.3) + real per-library fetcher gating/ordering

### Performance
- Sample-count windows + scan reuse — publish drops from ~18-21h to ~4-6h
- Drive user-data filters through the BaseItems PK, not a correlated EXISTS
- /Persons dedup via one covering-index aggregate pass + SQL paging

### Testing
- Cover the 0.5 settings builders + provider-name guard

### Results
- First open-loop v2 run record (run-2e894f3)

## [0.29.0] - 2026-08-16

### Bug Fixes
- Analysis-review fixes — drain deadlock, decode timeout, reserved keys
- Analysis-review round 2 — the doc edits that never landed, and four real fixes

### Documentation
- Analysis-review round 3 — the four small landings

### Features
- ABI 0.4.0 — the generic media-analysis capability

### Style
- Hoist the PermissionsExt import (items-after-statements)

## [0.28.1] - 2026-08-16

### Bug Fixes
- Run index.html transforms on the bare /web/ directory request

## [0.28.0] - 2026-08-16

### Bug Fixes
- Fingerprint with ffmpeg's chromaprint muxer

### Features
- Rolling updates by default

## [0.27.0] - 2026-08-16

### Bug Fixes
- Resolve a person filmography by item id, not just Peoples row id
- Capability-review round 1 — token leak closed, coverage restored

### Features
- WIT 0.3.0 — plugin routes, web transforms, rich queries, KV state
- 0.3.0 finishing pass — transform proof, wasm-hello demos, docs
- Declared egress — plugin-shipped public-network allowlist

### Testing
- Dedupe test Config via Config::test_stub
- 0.3.0 capability proof — KV caps, plugin routes end-to-end, uninstall state cleanup

## [0.26.0] - 2026-08-14

### Bug Fixes
- Harden repository install per review
- Close guid-squatting + provenance gaps from review round 2
- Round-3 review — ledger at the source, file-drop squat door
- Cap + time out repository fetches (round-4 review)
- Settings-page hardening from review round 5
- Round-8 minimal — id rule holds within the incoming batch

### Features
- Jellyfin-style repository install for WASM plugins
- Plugin settings pages — config-pages export + synthesized fallback (ABI 0.2.0)
- Round-7 hardening — disable disarms pages, immutable versions, DNS pin

## [0.25.2] - 2026-08-14

### Bug Fixes
- Make credits detection survive a small /tmp, and visible when it fails

## [0.25.1] - 2026-08-14

### Bug Fixes
- Cast device list, and chapter images after a scan

## [0.25.0] - 2026-08-13

### Bug Fixes
- Harden the plugin host per external review
- Untrack the perf-fixtures symlink, widen the private-IP deny, tidy the message
- Deny http-fetch during load; cache the event enabled-flag
- Serve chapter thumbnails — DTO tag + image route
- Library tiles sample the right kinds, after artwork lands

### CI/CD
- Build the WASM example guest and gate ferrofin-wasm coverage

### Documentation
- Two-tier plugin architecture, sandbox security first
- WASM plugin tier — sandbox model, authoring, knobs
- The WASM capability surface after E2
- Sweep the plugin story across CLAUDE.md, ARCHITECTURE, FEATURES, CONFIG, README
- Spell out the WASM memory ceiling semantics and real cost
- Confirm the 128 MiB per-plugin memory default

### Features
- Add the Tier-1b WASM plugin host (ferrofin-wasm)
- Load WASM plugins at the composition root
- Add the wasm-hello reference guest (toolchain island)
- E2 capabilities — http-fetch, query-items, write-media-segments
- E3 — WASM plugins as scan metadata sources (metadata-lookup)
- Deny private/loopback http-fetch by default, per-plugin allowlist
- Log session start/end and lockouts to the activity feed

### Refactor
- Fold provider-id merging into the dynamic metadata helper

### Testing
- Server-level HTTP test for WASM plugins; share the WAT fixture

## [0.24.1] - 2026-08-13

### Bug Fixes
- Episodes sort by number, not title — restores the play queue

## [0.24.0] - 2026-08-13

### Bug Fixes
- Kill the spinner class — index merged-version lookups, drop unknown enum tokens
- Episode counts exclude merged alternate versions
- Episode playback returns — startItemId id-casing, 3000x faster user-data sort, merged-version sources
- Honor the folder year when matching a series on TVDB
- Don't emit path-keyed MusicArtist rows

### Documentation
- Mark Live TV + DVR as not yet human-verified end-to-end

### Features
- Enforce the CollectionManagement policy on collection routes
- Populate the dashboard activity log with system events
- Download studio thumbs during the scan
- Persist and serve RemoteTrailers
- Album artwork from embedded cover art
- Artist → album → track hierarchy in the music scan
- Detect disc structures and record VideoType

## [0.23.0] - 2026-08-13

### Bug Fixes
- Person favorites round-trip — one Person item per name, Jellyfin-derived ids
- User-data sorts order for real — DatePlayed, PlayCount, Release fallback
- Date Added and Parental Rating sorts get real data at scan time
- Audio DTOs carry AlbumId; performer links prefer the browsable artist id

### Features
- The movie scan resolves extras into owned rows

## [0.22.0] - 2026-08-13

### Features
- Library tiles get real artwork — post-scan CollectionFolder collage

## [0.21.2] - 2026-08-13

### Bug Fixes
- FMP4 init serving never cancels the transcode start — fatal fragParsingError regression

## [0.21.1] - 2026-08-13

### Bug Fixes
- Filter facets reach Jellyfin coverage — provider merging, clean-value dedup, edit re-indexing
- Next Up returns — playback start stamps LastPlayedDate

## [0.21.0] - 2026-08-13

### Bug Fixes
- Collections become visible — BoxSet browses re-root onto linked-child ancestors
- Music libraries populate — scoped post-add scan, tag-derived track names

### Features
- Transcode and playback logs name the media, not just ids

## [0.20.1] - 2026-08-13

### Bug Fixes
- Jellyfin-web parity sweep — filters, favorites, latest rows, legacy routes, DTO gaps
- Transcode jobs die with their consumers; segment retries wait instead of killing
- Provider episode titles replace the filename placeholder; SeriesName comes from the series row
- Extract video frames into the temp dir, not beside the media

### Miscellaneous
- Fix cargo fmt issue

### Testing
- Cover the parity-sweep and transcode-lifecycle fixes

## [0.20.0] - 2026-08-13

### Bug Fixes
- User metadata edits survive library scans (honor IsLocked)
- Uploaded images survive the scan's artwork rewrite
- Scope the episode merge key to the series row, not its name

### Features
- Log why a websocket session is anonymous

### Miscellaneous
- Remove legacy implementation-plan docs

## [0.19.4] - 2026-08-12

### Bug Fixes
- Stop the library scan from clobbering columns it does not own
- Merge/split write their link column instead of full stale rows

### CI/CD
- Free runner disk and strip debuginfo so jobs fit GitHub-hosted runners
- Source homelab infra endpoints from CI/CD variables

### Miscellaneous
- Stop tracking .claude/ and .mcp.json (local agent tooling)

## [0.19.3] - 2026-08-12

### CI/CD
- Make base-image builds resilient to github.com flakiness

### Documentation
- Phase 5 — README front door, feature matrix, extensions, config
- Note the pre-publication history rewrite

### Refactor
- Rename Hermit → Ferrofin across the codebase

## [0.19.2] - 2026-08-12

### CI/CD
- Skip unused Dockerfile stages under kaniko (fix release web rebuild)

### Testing
- Allowlist PlaySessionId in the no-lowercase-GUID invariant

## [0.19.1] - 2026-08-12

### Bug Fixes
- Stop the 0007 rebuild from cascade-deleting all user data
- Complete 0007's GUID-uppercase coverage (audit follow-up)

### Testing
- Verify + classify the 8 flagged ledger rows
- Unit-test the extension crates and gate their coverage

## [0.19.0] - 2026-08-12

### Bug Fixes
- Give episodes their cast — merge series regulars + surface roles
- Detect BOM-less UTF-16 + restore the -sub_charenc hint

### Documentation
- Drop stale PORT_REPORT.md reference from contract-superset doc
- Promote the knowledge base's load-bearing docs into public docs/

### Features
- Store GUIDs and datetimes in Jellyfin's exact text formats
- Pin the schema to Jellyfin 10.11.8 — migration 0007 + code convergence
- BaseItems.Data JSON is the playlist/collection source of truth
- Adopt an existing Jellyfin 10.11.8 database in place

### Miscellaneous
- Release hygiene — community files, self-contained builds, de-identification

### Testing
- Schema-conformance gate + the drop-in round-trip test

## [0.18.0] - 2026-08-10

### Features
- Live filesystem watching of library roots (inotify via notify)
- Debounce filesystem changes behind LibraryMonitorDelay
- Path-scoped ingest — resolve just the changed files

## [0.17.0] - 2026-08-10

### Bug Fixes
- Report CanUninstall=true so the dashboard shows the enable/disable toggle
- Correct transcode.js fixture path after the suite/ reorg

### Features
- Accumulate benchmark reruns per SHA instead of overwriting
- Prune items whose files were deleted from disk
- Push UserDataChanged to the user's other devices
- Send the WebSocket pushes Jellyfin clients rely on

## [0.16.0] - 2026-08-08

### Bug Fixes
- Scope per-library refresh to that library only

### Features
- Fanart.tv artwork + scan-time provider ids + audio tags
- Materialize MusicArtist items from album-artist tags
- Add MusicBrainz client (ws/2, throttled)
- Resolve MusicBrainz ids for music items (post-scan pass)
- AudioDb + fanart music leg + provider client tests

### Testing
- Cover the absent-plugin 404 branch

## [0.15.0] - 2026-08-07

### Bug Fixes
- Cap transcoded audio channels per encoder

### Features
- Add Studio Images and TheTVDB metadata providers

## [0.14.1] - 2026-08-07

### Bug Fixes
- Stop rendering subtitles twice (burn-in + client overlay)

## [0.14.0] - 2026-08-07

### Bug Fixes
- Derive SortName from the name for by-name items

### Features
- Build movie recommendations from watch state
- Port MergeVersions as a full extension (upstream 12.0)

### Refactor
- Consolidate benchmark/ + parity/ into the one suite/ folder
- Move raw SQL behind a repository boundary

## [0.13.0] - 2026-08-06

### Bug Fixes
- Apply NFO <title> as the authoritative item name
- Match Jellyfin's default + CustomPrefs serialization
- Fall back to metadata folder when media is read-only
- Stamp image date_modified from file mtime, not scan time
- Match Jellyfin's by-name item shape (Genre/Studio/Person)
- Count a Person's credited items via the People map
- Stop the transcode kill/restart storm when the source keeps failing

### Features
- Lossy WebP output via libwebp (Skia parity)
- Port the weighted similarity scorer (item_similar*)

### Testing
- Dedupe raw test SQL back under the sql_boundary ceilings

### Skill
- Add final helm-render compatibility review

### Style
- If/else over single-pattern match (clippy)
- Rustfmt the WebP encoder + collage-test formatting

## [0.12.1] - 2026-08-06

### Bug Fixes
- Hand off k6's provisioned token for post-load captures
- Error panels read 0 instead of "no data" when there are no errors

### Performance
- Run image encode/resize on spawn_blocking
- Bound concurrent image encodes to core count

### Testing
- Guard get_playlist_items link order

### Bench
- Clean release record v0.12.0 / de2dc00 (parity + perf)

### Chart
- Add de-identified values.example.yaml + maintenance skill

## [0.12.0] - 2026-08-05

### Features
- Instrument scheduled-task runs with a root span
- Instrument library scans with a trigger-tagged root span
- Log DB pool open and migration head (Step 2)
- Make library-scan progress cadence configurable
- Add the auth security-audit trail
- Instrument transcode jobs + log ffmpeg's exit
- Span the intro-skipper run + websocket session
- Provider fetch failures + published-URL resolution

## [0.11.0] - 2026-08-05

### Documentation
- Add RULES_LOGGING pointer to CLAUDE.md

### Features
- Add golden-signals + deep-dive Grafana dashboards
- Retention, non-blocking writer, panic hook, shutdown reason

### Testing
- Assert JSON log lines carry the span trace_id

## [0.10.0] - 2026-08-05

### Bug Fixes
- Report progress + log per-season so a stalled run is visible

### Features
- OTLP trace export with log↔trace correlation
- Redact secret strings behind a Secret newtype

## [0.9.0] - 2026-08-05

### Features
- Add OpenTelemetry-backed Prometheus /metrics endpoint
- Add HERMIT_ENABLE_METRICS bootstrap override

### Miscellaneous
- Add run-benchmark skill
- Add missing crate dependency
- Add secrecy crate dependency to hermit-providers and hermit-model

### Performance
- Serve user DTOs from the auth cache instead of 2-3 DB round-trips per request
- Reuse a pre-minted token for post-load captures

### Refactor
- Give subsystem crates typed errors that convert into ServiceError

### Bench
- Extend the surface to write paths — 4 POST variants + write-row comparability
- Release suite record 6d6f32f (parity + perf)

## [0.8.6] - 2026-08-04

### Bug Fixes
- Maintenance can no longer break live playback

### CI/CD
- Prebuild the ffmpeg runtime base so the service image stops re-running apt

### Bench
- Expand the surface 83 -> 114 variants; honesty check goes cross-server

### Suite
- First run on the expanded 114-variant surface

## [0.8.5] - 2026-08-04

### Bug Fixes
- Compute UserData.PlayedPercentage for in-progress leaf items

### Performance
- First real baseline + proven detector + absolute jitter floor
- Kill the per-request SQLite tax on the authenticated hot path
- Port Jellyfin's poster caching contract — fixes slow poster loads on TV clients
- Rebaseline on the auth+images+played-percentage tree

### Parity
- Refresh ledger from the plan-8 suite run; drop the stale pre-plan-7 seed

### Suite
- First real seed data on the settled tree (plan 8 step 6)
- Drop the stale 0157db4 entry from runs.json too
- Carry HLS play-start TTFS into the merged record and viewer

### Viewer
- Carry the retired dashboards' visual design forward
- Run selection up under the title; footprint as comparison cards

## [0.8.4] - 2026-08-03

### CI/CD
- Best-effort perf-gate job — self-contained merge-base comparison

### Documentation
- Perf/parity remediation plan set (plans 1-7, as executed)
- Plan 8 — close out plan 3/4/6 deviations (single-agent)
- Accepted-deviations ledger; ignore __pycache__ and stray result logs

### Performance
- One projection path — single item is a batch of one
- Add Hermit-only p50/p95/p99 regression gate (plan 4)
- Unify parity + benchmark into one cross-referenced suite
- Split reader/writer pools; pool size becomes a config knob (auto = cores)
- One comparator, one baseline — suite/gate.py absorbs perf-gate.mjs

### Bench
- DB pool sweep harness + realistic-load phase D
- Finish the bring-up consolidation onto suite/lib.sh

### Viewer
- One dashboard — suite viewer absorbs :8123 and :8124

## [0.8.3] - 2026-08-03

### Bug Fixes
- Install fpcalc (libchromaprint-tools) in the benchmark image

### Performance
- Stop materializing id sets and binding giant INs
- Push pagination, name filters, and total-count into SQL
- Chunked guide inserts + SQL boundary ratchet test
- Skip always-zero played/child counts for by-name rows

## [0.8.2] - 2026-08-03

### Bug Fixes
- -noaccurate_seek on seeked video-copy streams (audio led video)

### Performance
- Batch folder played/total counts into two grouped joins

## [0.8.1] - 2026-08-03

### Bug Fixes
- Don't wipe resume point on a positionless stop report
- Seed the fMP4 init transcode at the resume offset

## [0.8.0] - 2026-08-03

### Features
- Switch to jellyfin-ffmpeg7 for NVENC/tonemapx support

### Testing
- Layer-3 binary/asset differential (images, fonts, css)

## [0.7.1] - 2026-08-03

### Bug Fixes
- Accept session commands like Jellyfin (no remote-control gate; optional GeneralCommand fields)
- Serve GET /System/Configuration/Branding (was 405)
- Run QuickConnect under its own DeviceId (don't clobber harness session)

### Testing
- Session remote-control journey (12 ops)
- Admin/config/device/auth write journeys + device reads
- Subtitles upload, GET /Devices/Options, MergeVersions controller probe
- QuickConnect handshake journey — untested reaches 0
- Make DELETE /Items bulk check status-based (Jellyfin deletes async)

### Parity
- Classify not-testable-via-differential ops (destructive/wizard/host-fs/Jellyfin-bug)
- Classify MergeVersions extension, Items/Root, Years/{year}, music by-name

## [0.7.0] - 2026-08-03

### Bug Fixes
- Populate SeriesName/SeasonName + folder UnplayedItemCount

### Features
- Read local Kodi/XBMC NFO sidecars during the library scan

### Testing
- Legacy playstate, password, vfolder rename journeys; classify LiveStreams
- VirtualFolders CRUD journey + resolved-param GET reads
- Ping/refresh/content-type journeys + Playlists GET read-backs

### Parity
- Classify the not-testable-via-body-diff surface (untested 259->83)

## [0.6.2] - 2026-08-02

### Bug Fixes
- Surface NowPlayingItem + PlayState from playback reports
- Include merged alternate versions in GetStaticMediaSources
- Install fpcalc (libchromaprint-tools) for intro skipper

### Miscellaneous
- Refresh ledger — session NowPlayingItem/PlayState fix lands (deep-verified 110)
- Refresh ledger — MergeVersions/AlternateSources fix lands (deep-verified 115)

### Performance
- Cap default SQLite pool by cgroup CFS quota, not just affinity

### Testing
- Write journeys for item-delete, session capabilities, path validation, video merge

## [0.6.1] - 2026-08-02

### Bug Fixes
- Stop emitting -maxrate/-bufsize (and -r) twice
- Audio downmix parity — libfdk_aac preference + volume=2 boost

### Documentation
- CLAUDE.md toolchain pin 1.97.0 -> 1.97.1 (match rust-toolchain.toml)

### Miscellaneous
- Classify playstate-progress + playlist-share-delete as methodology (not Hermit bugs)

### Performance
- Use jellyfin-ffmpeg's tonemapx for software HDR tonemap

### Testing
- Expand write journeys — API keys, UserData, DisplayPreferences, task triggers, device options, playlist move
- Write journeys for playstate, capabilities, user/system config, playlist share
- Reorganize integration tests by domain, not batch

## [0.6.0] - 2026-08-02

### Bug Fixes
- Name the cause when media reads fail at playback
- Deleting a box-set/playlist must not delete its linked members (data loss)
- 6 real GET divergences — LiveTv defaults, server config, Users/Public, Items/Counts
- Vendor Jellyfin's localization data (countries, US ratings, UI options)
- LiveTv/Info service visibility+tuners, MetadataEditor ContentTypeOptions

### CI/CD
- Bump rust CI + build images to 1.97.1 (match rust-toolchain.toml)

### Documentation
- Round-2 triage verdicts — 12 real bugs in the 13 flagged GETs

### Features
- Deep-diff the whole GET surface in the sweep (untested 361->280)

### Miscellaneous
- Refresh ledger after data-loss fix — write journeys 19/19, deep-verified 89
- Refresh ledger after batch-3 fixes + classify residual instance noise
- Refresh ledger — localization vendoring lands, deep-verified 95
- Refresh ledger + classify item-edit (oracle) & MetadataEditor (ExternalIdInfos)

### Performance
- Default the SQLite pool to available parallelism

### Testing
- Expect ItemCount=0 in counts_group_by_kind
- Expect empty ContentTypeOptions for a plain item

### Bench
- Isolate phase runs in their own compose project + ports

### Parity
- Denylist per-instance fields + classify confident not-bugs (flagged 49->15)

## [0.5.3] - 2026-08-02

### Bug Fixes
- V-prefix docker image tags to match git tags

### Performance
- Prebuild jellyfin-web into a CI image; stop webpacking per build

### Testing
- Lock WI-6 — collection members visible via parentId browse

### Bench
- Don't let a server's scan crash abort the whole run
- Retry scan up to 3× so an intermittent Jellyfin OOM isn't fatal

### Parity
- Mark the not-bugs as accepted classifications (curated, highest precedence)

## [0.5.2] - 2026-08-02

### Bug Fixes
- Always emit ImageTags as {} instead of omitting it
- Early-triggerable release gate via a needs-a-manual-job pattern
- Extract startup banner from run() to satisfy too_many_lines
- Batch child counts in upstream shape with LinkedChildren precedence
- Always emit ImageBlurHashes as {} (BaseItemDto null-field sweep)

### Miscellaneous
- Split the startup banner into grouped lines

### Bench
- Fix endpoint loop — name escaping, stdin, abort hardening

## [0.5.1] - 2026-08-02

### Refactor
- Extract version generation to scripts/version.sh

## [0.5.0] - 2026-08-02

### Bug Fixes
- Batch 1 — image cache tags, Filters2 shape, playlist id round-trip, RefreshStatus
- Stamp MessageId on every outbound WebSocket message
- Batch 2 — MediaSource fields, drop IsOriginal, CanDelete, BoxSet browse, Playlists folder
- Release by tagging the commit, not committing to main
- Keep create-release manual; gate it on lint+coverage via needs

### Build
- Default the reported version to git describe, not a stale constant

### CI/CD
- Publish describe-style dev images for build-worthy commits
- Gate :latest solely on a final release tag at the push site
- Cancel the redundant dev image build when a release is cut
- Allow triggering create-release early, auto-cancel if tests fail

### Documentation
- Fix env example to HERMIT_LOG (the var Hermit reads)
- Triage roadmap + per-op verdicts from the parity-triage workflow

### Features
- Chart-managed env ConfigMap injected into Hermit

### Miscellaneous
- Refresh ledger after batch 1+2 fixes; denylist ETag (instance hash)

### Bench
- Isolated open-model per-endpoint harness with cgroup CPU cost
- Auto-detect new runs, auto-refresh, and notify
- Per-endpoint saturation sweep for max sustainable RPS
- Mixed contention run + cgroup memory.peak footprint

## [0.4.1] - 2026-08-02

### Bug Fixes
- Write server logs to disk and stop dropping client uploads
- Stop skipping the test stage on non-image commits

### CI/CD
- Add prebuilt Rust CI base image with nextest/llvm-cov baked in

## [0.4.0] - 2026-08-02

### Bug Fixes
- Report image file Size in GetItemImageInfos (parity)
- Serialize MediaStream computed properties + map NalLengthSize (parity)
- Advertise ProductName as "Jellyfin Server" for client compatibility

### CI/CD
- Build image only on release tags; gate create-release on lint+coverage
- Run tests with cargo nextest (keep cargo test --doc for doctests)

### Features
- Layer-2 read depth with id-correlation (Phase 1 task 6)
- Persist per-field read diffs; image Size fix reflected in ledger

### Miscellaneous
- Refresh ledger after media-info fix (Items/{id} missing 170->70)

### Performance
- Assemble list media sources from prefetched streams; single-pass Filters2 languages
- Page-batch cast/crew people and their images in list DTO projection
- Page-batch studio/genre/artist value-id resolution in list DTO projection
- Page-batch chapters and trickplay in list DTO projection

## [0.3.0] - 2026-08-01

### Bug Fixes
- Report the real release version and let CI own the bump
- Run create-release on alpine/git, derive bump in shell
- Serde-default TypeOptions so jellyfin-web VirtualFolders bodies deserialize
- Serde-default remaining client-body DTOs (TypeOptions-422 audit)
- Anchor breaking detection to the CC footer; cap 0.x at minor
- Derive the semver bump from the commit subject only

### Features
- Per-operation parity ledger + Layer-1 sweep + Layer-2 write journeys

### Performance
- Batch N+1 query loops in by-name, item-detail, and user DTO paths
- Derive recommendation DtoOptions from fields; page-batch DTO media streams + provider ids

### Testing
- Add multi-version results viewer and expand endpoint coverage
- Send realistic jellyfin-web library body so the sweep catches serde-422s

## [0.2.4] - 2026-08-01

### Bug Fixes
- Emit ChildCount on latest items; denylist per-session instance noise
- Seed admin preserves per-user policy defaults instead of blanking them
- Default new users to Jellyfin's HidePlayedInLatest + CastReceiverId
- Persist EnableContentDeletion + EnableRemoteControlOfOtherUsers permissions
- Report IsActive=true for a controller-less session
- Emit session Capabilities/PlayState/queues; denylist task run-times
- Ship Jellyfin's two built-in cast receiver applications
- Compute SortName during the scan (Jellyfin CreateSortName)

### Features
- Response-body parity harness + first fixes (Fields, folder-movie naming, blurhash)
- Populate DTO field defaults + honour fields on /Items/Latest
- Compute image dimensions + blurhash during the scan
- Serve the full ISO-639 culture list
- Bundle pinned jellyfin-web client at /usr/share/hermit/web

## [0.2.3] - 2026-08-01

### Bug Fixes
- Stop create-release SIGPIPE on tag lookup, set tagger identity

### CI/CD
- Add manual create-release job that cuts the tag
- Replace GitLab cache with sccache-on-Garage

## [0.2.2] - 2026-07-31

### CI/CD
- Make the git tag the single source of truth for versioning

## [0.2.1] - 2026-07-31

### Miscellaneous
- Cargo fmt

## [0.2.0] - 2026-07-31

### Documentation
- Use registry.mangoleafstudios.com host

### Features
- Official Helm chart + OCI publish on release

### Miscellaneous
- Bump rust toolchain to 1.97.1

### Release
- V0.2.0

## [0.1.0] - 2026-07-30

### Bug Fixes
- Port 5 wire DTOs documented but never defined in Wave 1
- Remove SPA fallback that black-screened the web client
- Report Jellyfin API version 10.11.8, not the crate version
- First-time-setup auth for the localization endpoints
- Seed a passwordless admin so the setup wizard works
- First-time-setup auth for library-structure + environment
- Case-insensitive API routing (match Jellyfin/ASP.NET)
- First-time-setup auth for /Libraries/AvailableOptions
- Build auth-result User DTO via get_user_dto; persist server id
- Populate SystemInfo.OperatingSystem (was null)
- Default ValidatePathDto.validate_writable (was 422)
- Case-fold query-param keys (Jellyfin is case-insensitive)
- Resolve LibraryOptions POST by CollectionFolder Id (was 404)
- Self-heal missing CollectionFolder row (scan FK 500)
- Quote-aware ffmpeg/ffprobe arg splitting
- Accept jellyfin-web bodies that strict serde rejected (422s)
- Transcode incompatible audio (no-audio on playback)
- Resolve codec preference list + number seek segments
- Evict stale transcode on seek so scrubbing doesn't corrupt
- Align seek-segment timestamps to the playlist (-output_ts_offset)
- Report each library's CollectionType so TV renders as shows
- Deliver HEVC as fMP4 tagged hvc1 so HDR HEVC plays
- Force fMP4 HLS container for HEVC/AV1 (was mkv 404)
- Downmix HLS transcode audio to the profile channel cap
- Transcode to h264, not the client's first codec preference
- Render season/episode artwork (default DtoOptions.image_types to all)
- Legacy favorite/played/rating routes + aired episode order
- DELETE /Items/{id} no longer 500s on playlists/collection members
- Honor StartItemId on /Shows/{seriesId}/Episodes
- Detect Dolby Vision so DV content transcodes for browsers
- Extract + persist embedded chapters for the playback timeline
- Honor fillWidth/fillHeight for unknown-size images (poster clipping)
- Populate MediaStream DisplayTitle (was 'Undefined')
- Write ItemValues for genres/studios/tags (fixes More Like This)
- Deliver + burn in selected subtitles (were never rendered)
- Write-first transactions to end SQLITE_BUSY_SNAPSHOT + AncestorIds 500
- Populate Episodes/Genres/Studios tabs + Suggestions
- Map series studios from broadcast networks, not production companies
- Report real disk free/used for every folder
- Wire real ffmpeg subtitle encoder (external VTT delivery)
- Materialize Person items + cast/crew artwork; async scan
- Apply PersonIds/Person filter so a person page lists their work
- Run intro-skipper analysis in the background
- Persist cleared like, fix Latest filters + media folders
- Make subtitle delivery, burn-in, and extraction actually work
- Serve the livetv named configuration instead of 501
- Accept repeated includeSegmentTypes query params
- Stamp HasSegments on media sources
- Fingerprint the full credits window, not fpcalc's 120s default
- Queue started tasks so Running state reflects the real run
- BENCH_SKIP_BUILD knob; stop RSS sampling before the transcode phase
- Stamp derived CleanName on save so items are searchable
- Merge repeated query params so array params don't 400

### CI/CD
- GitLab pipeline — semver-tagged docker images to the registry
- Build the release image with kaniko instead of docker-in-docker

### Documentation
- Add CLAUDE.md contributor & agent guide
- The client now fetches metadata, not just artwork

### Features
- Scaffold hermit workspace + Wave 0 port
- Port MediaBrowser.Model DTOs and enums
- Wave 2
- Port Jellyfin.Database schema + entities (sqlx + SQLite)
- Port MediaBrowser.Controller interfaces (the DI seam)
- Wave 5 impl crates
- Port core manager implementations (the workhorse)
- Port Jellyfin.Api — contract-complete axum layer
- Implement 236 endpoints for real (Wave 7b)
- Real HLS transcode pipeline + more endpoints
- Composition root — Hermit boots as a real server (Wave 8)
- Implement 31 of the 35 core-not-yet-wired routes
- Serve a static web client at /web
- Session WebSocket at /socket (fix "Connection Failure")
- Tier-1 compile-time plugin system + plugin-manager API
- Named config sections, Backup list, legacy Users/{id}/Items
- Create CollectionFolder item on library add (scan A1)
- Persist all POST /System/Configuration/{key} (was 501)
- Filesystem scan engine — movies → item rows (scan A2)
- Make library scan triggerable from the UI
- TV + music scan resolvers (Series/Season/Episode, Album/Audio)
- Probe media on scan (streams/duration/size) + PlaySessionId
- Path-scoped /Users/{userId}/Items/{Resume,Latest,itemId} routes
- RemoteAccess persist, real System/Endpoint, QuickConnect token
- Populate the plugin catalog from repository manifests
- Serve resized/converted images (was full-size original)
- Persist playlist shares (/Playlists/{id}/Users)
- Synchronized group playback over the session WebSocket
- Add SubtitleProvider seam for the provider registry
- OpenSubtitles provider + enriched search request
- NVENC hardware transcoding (full-GPU 4K/HDR)
- TV episode linkage + automatic TMDB artwork
- TMDB season posters + episode stills
- Persist artwork upload/delete + user avatar
- Read/write local .lrc/.elrc/.txt sidecars
- Implement drag-to-reorder (move_item)
- Ship the per-type MetadataOptions defaults
- TMDB remote metadata search + image browsing
- Honor insert position on POST /Playlists/{id}/Items
- M3U tuner + XMLTV guide parsers
- Schema for tuner hosts, listing providers, channels, programs
- Real DB-backed LiveTvManager + composition-root wiring
- Manager-backed LiveTv handlers + config/lookup routes
- Channel live-stream playback (Phase 8)
- DVR timer/series-timer/recording CRUD (Phase 9)
- Fetch full movie/series metadata from TMDB on scan
- Fetch person profile photos + trailers in details()
- Log playback to the dashboard activity feed
- Enrich cast/crew with TMDB biography, birthday, birthplace
- Rotten Tomatoes critic rating via OMDb
- Plan + hermit-chromaprint pure intro/credits math
- Intro Skipper — audio-fingerprint intro/credits detection
- Implement four hollow functional gaps end-to-end
- Dashboard settings page for plugins (Intro Skipper)
- Add Enable toggle to the Intro Skipper settings page
- Implement the last 5 Live TV routes; fix api-status audit
- Full Intro Skipper settings parity (9 tabs, ~70 config fields)
- Implement the full Intro Skipper plugin API surface
- Real metadata-provider registry for AvailableOptions
- Vendor real plugin web UIs; add the File Transformation extension
- Enforce ownership, shares, and open-access for real
- Real per-item metadata refresh, incl. seasons/episodes
- Working Hermit-vs-Jellyfin harness, first run green
- The full Jellyfin dashboard task set + a real trigger scheduler
- Real transcode TTFS — copy + forced-encode modes; fix 30s transcode start timeout
- Honor the negotiated bitrate/resolution caps — downscale, -maxrate, HDR tonemap
- Record playback decisions to PlaybackSessions (Track A)
- Bus-registered sockets are remote-controllable

### Miscellaneous
- Gitignore .rcg/
- Lockfile for hermit-networking dependency
- Fake MediaSourceManager impls + Cargo.lock for refresh_media_streams
- Refresh Cargo.lock for hermit-providers tokio/tracing deps
- Add hermitcodegraph server entry

### Performance
- Halve time-to-first-segment (temp_file + drop index+1 wait)
- Small SQLite pool + batched DTO relation loads
- 3s HLS segments + forced segment-boundary keyframes

### Style
- Rustfmt the person_ids filmography test
- Rustfmt the extensions registered_plugins builder

