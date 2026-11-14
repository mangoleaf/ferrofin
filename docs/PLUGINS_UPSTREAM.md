# Third-party plugin upstream tracking

One row per Jellyfin plugin ported into Ferrofin as a compiled-in extension
(`crates/ferrofin-extensions/`). This is the single source of truth for *which
upstream revision each port is based on*.

Syncing with upstream: clone the upstream repo, `git log <ported rev>..HEAD`, port each
behavioural change (or classify it as not-applicable / accepted divergence in the notes
below), then bump **Ported rev** and the matching asset pin.

Conventions:
- **Ported rev** — the upstream commit the Ferrofin implementation is faithful to.
  Bump it only after the delta is actually ported (or classified as
  not-applicable/accepted-divergence below).
- Dashboard-asset pins live in `crates/ferrofin-extensions/build.rs`
  (`*_REPO`/`*_REV` consts) and must be kept equal to **Ported rev**.

| Plugin | Upstream repo | Ported rev | Upstream version | Status |
|---|---|---|---|---|
| Intro Skipper | https://github.com/intro-skipper/intro-skipper | `db09359` | 10.11/prerelease | ported |
| File Transformation | https://github.com/IAmParadox27/jellyfin-plugin-file-transformation | `f4f01c3` | 2.5.10.0 | ported |
| Merge Versions | https://github.com/danieladov/jellyfin-plugin-mergeversions | `e6f58d6` | 12.0.0 | ported |
| LDAP-Auth | https://github.com/jellyfin/jellyfin-plugin-ldapauth | `d11c86a` | 24 | ported |

## Per-plugin notes

### Intro Skipper
- Ferrofin files:
  - the extension: `crates/ferrofin-extensions/src/intro_skipper.rs` (tasks,
    config, caches, the analysis seam) and `intro_skipper/` —
    `queue.rs` (`QueueManager`), `engine.rs` (`BaseItemAnalyzerTask`,
    `ChromaprintAnalyzer`, `TimeAdjustmentHelper`), `chapter_analyzer.rs`,
    `black_frame.rs` (`BlackFrameAnalyzer`, recaps), `credit_scenes.rs`
    (`CreditsBlackFrameAnalyzer` and `Analyzers/Credits`), `automatic.rs`
    (`Entrypoint`), `config_hash.rs` (`ConfigHasher`), test data `testdata/`;
  - processes: `crates/ferrofin-extensions/src/ffmpeg.rs` (`FFmpegService`,
    `FFmpegOutputParser`), `fingerprint.rs` (Chromaprint);
  - paged library reads: `crates/ferrofin-extensions/src/library_pages.rs`
    (shared with Merge Versions);
  - storage: `crates/ferrofin-core/src/intro_skipper_repository.rs`, migrations
    `0036`–`0038` (`Ferrofin*` tables), the seam
    `crates/ferrofin-traits/src/intro_skipper.rs`;
  - routes: `crates/ferrofin-api/src/handlers/intro_skipper.rs`, the
    first-episode filter in `handlers/media_segments.rs`; tests
    `crates/ferrofin-api/tests/intro_skipper_handlers.rs`;
  - vendored assets `crates/ferrofin-extensions/assets/introskipper/`.
- Ported: the segment store and its routes, the queue, incremental analysis
  under `ConfigHasher`, settled-season reanalysis, every analyzer (Chromaprint,
  chapters, black frames, credit scenes, recaps, anime previews), silence and
  keyframe snapping, `ProbeAudioDuration`, `ProcessPriority`/`ProcessThreads`,
  the detection cache, the Clean Intro Skipper Cache task, automatic analysis
  (`Entrypoint`) and `AnalyzeAgain`, `MediaSegmentsFirstEpisodeFilter`, the task
  identities and the support bundle. The upstream xUnit suites are
  transliterated beside the code they test.
- Accepted divergences (do NOT "fix" during sync):
  - **Storage:** the plugin's own database is not imported (owner decision D3);
    Ferrofin keeps its segments in `FerrofinIntroSkipperSegments` and publishes
    them to `MediaSegments` under the plugin's MD5 provider id (D1, D2). The
    detection cache is files beside the fingerprints (D7), not SQLite: prints
    are keyed by window in whole seconds without a settings hash (re-keying
    would re-fingerprint every library; a sub-second window change reuses the
    old print), and the clean task deletes cache files no current read hits.
    Migration `0037` seeds the segment store from the segments already
    published, as automatic rows with an empty hash (upstream's schema upgrade
    does the same): the first pass replaces each with what it detects, or
    deletes it. Hand edits made before `0037`, and the plugin's own records of
    them on an adopted database, are therefore not kept; nor are the plugin's
    settings (`IntroSkipper.xml`). `docs/UPGRADING.md` tells operators.
  - **Hashes:** Ferrofin's analysis hashes carry an analyzer-set token
    (`config_hash::ANALYZERS`), bumped when an analyzer lands, so earlier
    results are analysed again with it.
  - **Fingerprinting:** ffmpeg's `chromaprint` muxer, falling back to `fpcalc`
    (upstream has only the muxer); `IncompatibleFFmpegBuild` is raised only
    when neither exists.
  - **Upstream bugs not ported:** an anime preview is stored under the Preview
    hash (upstream's Credits hash lets the Preview pass delete it for good);
    Matroska `DURATION` tags parse at any fraction length (.NET's `TimeSpan`
    stops at seven digits).
  - **Failures:** a store write failure fails the season, retried next pass,
    in every analyzer (upstream's black-frame analyzers log it per episode and
    go on, the episode left analysed in memory); a failed keyframe
    detection keeps the end; an invalid or runaway chapter pattern matches
    nothing, reported once per pattern (upstream fails the pass); a negative
    adjustment window is logged once, not per segment.
  - **Concurrency:** the clean task holds the one-pass latch (upstream takes no
    lock and can delete segments a concurrent pass just stored).
  - **Automatic analysis:** library changes arrive through the event bus (D8),
    debounced by `LibraryUpdateDuration` and batched, so a batch with any
    addition waits as an addition, and image-only updates are not told apart.
  - **Media Segment Scan** (`TaskExtractMediaSegments`) runs detection (D5),
    then every registered segment provider — the plugin's `SegmentProvider`
    publishing what detection stored, as upstream's scan runs it.
  - **Support bundle:** the version line also names Ferrofin's; the ffmpeg
    checks run when the bundle is asked for (upstream: at start); the bundle
    names the Chromaprint backend. The queue counts come from the last
    whole-library queue (the scheduled pass, Clean Intro Skipper Cache, Clear
    excluded). An automatic analysis or a season rescan queues only its
    seasons (below), so a bundle asked for after one builds the whole queue
    once, without ffprobe, and counts it then; upstream counts every
    analysis's queue, so the two differ only when the library changed after
    the last automatic analysis.
  - **Task keys:** the detection task was `IntroSkipper.Detect` before D6; the
    task manager adopts what was saved under that key.
- **Memory** (implementation only, the behaviour is the same): upstream
  holds each library's list of episodes and movies while it queues them. The
  queue here takes a snapshot of the ids its query matches and reads the rows
  by id, `PAGE_SIZE` (100, upstream's `QueryPageLimit`) at a time: first to
  place each item in its library, then library by library to queue it. The
  items, their libraries and their order are the same, so the queue is the
  same. Rows that tie on all four sort keys keep whatever order SQLite's plan
  gives them; neither the snapshot nor the whole list promises one. An
  automatic analysis or a season rescan, which upstream gives the whole queue
  and then filters to its seasons, reads only those seasons' series and
  movies (each episode's ancestors hold its series) and probes only the items
  it analyses: the ids it reads keep the whole snapshot's order, and a
  season's entries depend only on its series' items, so its seasons are
  queued exactly as the whole queue has them (given the series link every
  scanned season and episode row carries).
- **File checks** (implementation only, the behaviour is the same): upstream's
  `VerifyQueueAsync` checks every queued file with `File.Exists` on every pass.
  Here a season's files are checked only when the pass can act on its items:
  some mode still to analyse, or `ReanalyzeSettledSeasons` on. Otherwise every
  mode stops at `HasUncachedAnalysisWork` and nothing is published or
  recorded, so which files exist changes nothing; only the debug line naming a
  missing file is not written.

### File Transformation
- Ferrofin files: `crates/ferrofin-extensions/src/file_transformation.rs`,
  vendored assets `crates/ferrofin-extensions/assets/filetransformation/`.
- Ahead of the pin: the `/web` response for a transformed file (`transform_web_file` in
  `apps/ferrofin-server/src/lib.rs`) follows `Infrastructure/FileTransformationMiddleware.cs`
  @ `0900de8f1`: an `ETag` of the first 8 bytes of SHA-256 over the transformed bytes,
  `Cache-Control: no-cache`, no `Last-Modified`, and an equal `If-None-Match` answered with
  `304`. At `f4f01c3` the static-file middleware derived the `ETag` from the source's
  modification time and the output's length, so a same-length change could be served stale.
- Accepted divergences:
  - **A failing callback leaves the file as it was.** The plugin writes a callback's body over
    the file whatever its status (`TransformationHelper.ApplyTransformation`), so an error
    page is served as the bundle, and a timeout or transport failure is an exception that
    fails the request. Ferrofin serves the file unchanged and logs the failure once at warn,
    then at debug until the callback works again.
  - **The pipeline's output is capped at 16 MiB** (`MAX_TRANSFORMED_BYTES`). The plugin reads
    a callback's whole answer and applies every search/replace. Ferrofin reads no more than
    16 MiB of an answer, and skips a step that would grow the file past 16 MiB, because the
    pipeline runs on every anonymous `/web` request. jellyfin-web's bundles are a few MiB.

### Merge Versions
- Ferrofin files: `crates/ferrofin-extensions/src/merge_versions.rs` (the whole
  plugin — `MergeVersionsExtension`, `MergeVersionsService`, both scheduled
  tasks, config, eligibility filters), vendored assets
  `crates/ferrofin-extensions/assets/mergeversions/`, the trait seam
  `crates/ferrofin-traits/src/merge_versions.rs`, and the thin HTTP handlers
  `crates/ferrofin-api/src/handlers/merge_versions.rs`. The single-group
  `merge_versions`/`remove_alternate_sources` core ops (backing
  `POST /Videos/MergeVersions` + `DELETE /Videos/{id}/AlternateSources`) stay
  in `crates/ferrofin-core/src/library_manager.rs` — those are core Jellyfin
  routes, not plugin surface.
- Ported at 12.0 semantics (`e6f58d6`): provider-first episode merge key
  (Tvdb→Tmdb→Imdb → numbers → title, case-insensitive), transitive
  version-group expansion, existing-primary-preserving primary selection,
  `LocationsExcluded` config + inactive-library eligibility filters, the two
  24-hour dashboard tasks, and the vendored settings page. Routes and tasks
  self-gate on the plugin's enabled flag (disabled → routes 404, tasks no-op).
- Divergence PENDING OWNER DECISION: the pinned plugin (`e6f58d6`,
  `MergeVersionsManager.cs:211-233`) writes a merge as OWNED local versions —
  each alternate gets `OwnerId` = the primary and the primary's
  `LocalAlternateVersions` lists it, so the save writes `ChildType` 2 rows —
  and its split (`:282-299`) unlinks local versions too. Ferrofin writes a
  merge as a linked version (`PrimaryVersionId` plus a `ChildType` 3 row, as
  core `VideosController.MergeVersions` does), never merges or splits an
  owned row, and leaves `OwnerId`/type-2 rows to the scanner, which owns the
  files it groups (`ItemPersistenceService::sync_local_versions`). An
  adopted database's plugin merges (owned, type 2) therefore read as local
  versions here. The linked-child reroute is not ported.
- Accepted divergences (do NOT "fix" during sync): Upstream's
  `Parallel.ForEach` fire-and-forget async merges (error-dropping) run
  sequentially in Ferrofin. The four bulk operations (the routes and both
  daily tasks) also run one at a time: upstream has no lock, so a merge and a
  split started together interleave their link writes; here a second call
  waits for the first. The routes keep upstream's plain `[Authorize]` (any
  signed-in user), unlike core `POST /Videos/MergeVersions`, which requires
  elevation: owner decision 2026-10-24. The episode merge key is scoped to
  the series *row* (`SeriesPresentationUniqueKey`), not the series name as upstream
  does: a show present in two libraries (hot/cold tiers) has two series rows
  with one name, and the name-scoped key merged episodes across them —
  hiding each alternate from its own series' list and skewing season counts.
  The bulk episode task self-heals links whose key no longer matches their
  primary's (unlink + regroup within the series). The key's title fallback (no
  provider id, no episode number) also carries the core resolver's version
  key read from the episode's file name (`GetEpisodeVersionKey`: `D{yyyyMMdd}`
  for a name by air date, `S{n}E{n}` for one with both numbers): upstream's
  `title:{SeriesName}:{SeasonName}:{Name}:{ProductionYear}`
  is the same for every airing of a show whose files are named by date —
  `IsByDate` clears the number, the name is the series', the year the
  series' — so the task merged a whole season of airings into one episode.
  Two copies of one airing still merge, as the core resolver groups them. The
  stored `PremiereDate` is not used: an NFO, or a row adopted from Jellyfin
  (server-local midnight), can store an airing's date on another day. A file
  name with no such key carries the file name without its extension,
  compared case-insensitively: the resolver's name cuts everything after a
  year or release clutter, so unnumbered, undated episodes such as
  `Show (1975) - Guest A` and `Show (1975) - Christmas Special` are both
  "Show", 1975, and upstream's key merged them. Two copies with one such name
  in different folders still merge, whatever their containers; copies whose
  names differ (`Pilot 1080p`, `Pilot 720p`) no longer do. Only a row with no
  path or no file name keeps upstream's key. The self-heal also undoes a
  group made by hand through `POST /Videos/MergeVersions` of two airings, of
  two copies only one of whose file names carries the key, or of two
  unnumbered, undated episodes whose file names differ; the playlist and
  collection entries that merge rerouted to the primary stay on it.
  For the same two-libraries reason as the series-row scoping, movie
  grouping is keyed by (owning library `TopParentId`, `Tmdb` id) rather than
  the `Tmdb` id alone: the same film held in a cold (NAS) and a hot (local)
  library is two intentional entries, and merging them hid one behind the
  other. The bulk movie task self-heals existing cross-library links between
  `Tmdb`-carrying movies, and a group's `GetAllAlternateVersions` expansion
  drops any member it reaches outside that group's library, so a partner that
  never reaches the scan (excluded location, inactive library) cannot be
  re-merged through a stale pointer; such a member is unlinked only when it
  points back into the scanned library, never when its own primary is also
  outside (that is the other library's group, which this scan could not
  repair). Accepted cost, deliberately not configurable: the
  separate-4K-library setup that upstream's Tmdb-only key served — one item
  offering the HD and 4K copies as versions — no longer merges, and a
  cross-library group made by hand via `POST /Videos/MergeVersions` (which
  is itself unscoped, by design) is undone by the next nightly run whenever
  both copies carry a Tmdb id.
- **Memory** (implementation only, the behaviour is the same): upstream
  holds the whole list of eligible movies or episodes during a bulk
  operation. Ferrofin reads it a page at a time (`library_pages.rs`: a
  snapshot of the matching ids, then the rows by id, `PAGE_SIZE` at a time)
  and keeps only the fields the grouping reads, so the groups and merges are
  the same.

### LDAP-Auth
- Ferrofin files: `crates/ferrofin-extensions/src/ldap_auth.rs` (the extension, the
  configuration, the `LdapDirectory` seam, the settings page's three tests and their service),
  `ldap_auth/directory.rs` (the `ldap3` client and its rustls configuration),
  `ldap_auth/provider.rs` (the sign-in, password-change and password-reset provider),
  `ldap_auth/provider/profile_image.rs` (profile images and their sync task),
  `ldap_auth/referrals.rs` (referral following) and `ldap_auth/adoption.rs` (the
  `LDAP-Auth.xml` import), vendored assets `crates/ferrofin-extensions/assets/ldapauth/`, the
  trait seam `crates/ferrofin-traits/src/ldap.rs`, the image download seam
  `crates/ferrofin-traits/src/remote_images.rs` (implemented in
  `crates/ferrofin-providers/src/remote_images.rs`), the wire types
  `crates/ferrofin-model/src/ldap.rs`, and the thin HTTP handler
  `crates/ferrofin-api/src/handlers/ldap.rs`. The live directory test is
  `crates/ferrofin-extensions/tests/ldap_directory.rs` (`FERROFIN_LDAP_TESTS=1`, OpenLDAP in
  docker; CI does not run it), and `contrib/ldap-test-directory/` starts a seeded directory to
  try a running server against by hand.
- Plugin, configuration and connection test (v24, `d11c86a`): `LdapPlugin` (GUID, name
  `LDAP-Auth`, version 24, `ConfigurationFileName` `LDAP-Auth.xml`, the settings page under
  its name),
  `PluginConfiguration` with upstream's defaults, `GetConnectionOptions` and its certificate
  callbacks, and `LdapController.TestServerBind` with
  `LdapAuthenticationProviderPlugin.TestServerBind`: the body is saved into the
  configuration first, then connect, StartTLS, bind (`Anonymous` for an empty password, as
  Novell's client binds) and a subtree count of the base DN with Novell's filter for an empty
  one, `(objectclass=*)`, each step `Testing...` until it passes and `Error` naming the
  failure. The route is administrators only (`RequiresElevation`), answers `404` while the
  plugin is disabled, and refuses a body without `LdapServer` or `LdapBaseDn`
  (`[Required]`) with ASP.NET's validation problem. With both `UseSsl` and `UseStartTls`
  set, StartTLS is still sent over the LDAPS connection, as Novell's client sends it, and
  the server's refusal is the StartTLS step's error (`ldap3` alone would skip it). A server
  given as an IPv6 address works over TLS too: the certificate is checked against the
  address. For a host name, a certificate with no DNS name among its subject alternative
  names is matched by its subject common name, OpenSSL's fallback, which .NET uses on
  Linux.
- Sign-in (v24, `d11c86a`): `LdapAuthenticationProviderPlugin.Authenticate`,
  `LocateLdapUser`, `ConnectToLdap` and `LdapUtils.SanitizeFilter`, as the authentication
  provider the extension contributes: `Name` `LDAP-Authentication`, stored id
  `Jellyfin.Plugin.LDAP_Auth.LdapAuthenticationProviderPlugin`, so an adopted Jellyfin's
  LDAP users select it. The bind DN searches the base DN's subtree for `[UsernameAttr,
  UidAttr]` with `LdapSearchFilter`, its `{username}` replaced ignoring case, or else
  `(&<filter>(|(<attr>=<name>)…))` over `LdapSearchAttributes` with spaces removed, split on
  commas (an empty item kept), the name escaped byte for byte as `SanitizeFilter` does; the
  first entry is used, with upstream's warning when there are more. The user is found by the
  entry's uid (the external-identity store), else by the username attribute, who is then
  linked; either way only when that user's `AuthenticationProviderId` is LDAP-Auth's, so a
  local account of that name is never taken (see the divergence on uid links below). The
  password is checked by binding as the entry on a connection of its own.
  A new user is created when `CreateUsersFromLdap` is on, with both provider ids,
  `EnableAllFolders` and, when that is off, `EnabledFolders` as configured (the strings as
  given, as `SetPreference` stores them), and linked; with it off, upstream's message and a
  `401`. A returning user follows a directory rename (`RenameUser`, case-sensitive compare).
  An entry without the username attribute is a `400`, as upstream's create or rename with a
  null name throws an `ArgumentException`. The provider is enabled only with the plugin and a
  server (D3);
  upstream's `IsEnabled` is always `true`. A new user's picture is taken from the entry (see
  Profile images).
- Password change (v24, `d11c86a`): `ChangePassword`, which the user manager calls for a user
  whose `AuthenticationProviderId` is LDAP-Auth's (`POST /Users/{userId}/Password`). With
  `AllowPassChange` off it is upstream's `NotImplementedException`, a `500`. Otherwise the user's
  entry is found by their Ferrofin name as sign-in finds it (`LocateLdapUser`; no entry is
  upstream's `401`), then the bind DN, on a connection of its own, reads the root DSE's
  `supportedExtension`: with `1.3.6.1.4.1.4203.1.11.1` it sends the RFC 3062 password modify
  operation for the entry's DN without the old password; else it replaces `LdapPasswordAttribute`
  with the new password; with that empty too, upstream's `InvalidOperationException` (`Password
  attribute is not set`, a `500`). The bind DN must be allowed to set users' passwords. The
  write goes to the configured server, also for an entry the search found behind a referral
  (see Referrals).
- Password reset (v24, `d11c86a`): the same provider is the password-reset provider
  (`IPasswordResetProvider`, registered through `Extension::password_reset_providers` under the
  same name and id), so the users it creates, whose `PasswordResetProviderId` is its id, have
  their forgotten-password requests sent to it. `StartForgotPasswordProcess` answers `PinCode`
  with `PinFile` set to `PasswordResetUrl`, `$userId` replaced by the user's id (or the entered
  name's `GetMD5` when it names no account; the user manager sends such a request to the
  built-in provider) and `$userName` by the entered name lowercased (`ToLowerInvariant`), both
  ignoring case and unencoded, as upstream; with no URL, upstream's `NotImplementedException`, a
  `500`. It is enabled under the same conditions as sign-in (D3).
- Adoption (v24, `d11c86a`; owner decision D7): on a boot where LDAP-Auth has no `config.json`
  yet, an adopted Jellyfin's `{program data}/plugins/configurations/LDAP-Auth.xml`
  (`BasePlugin<T>.ConfigurationFilePath`, written by .NET's `XmlSerializer`) is read with the
  server configuration's importer. Every setting comes across, the bind password included
  (never logged). Each `LdapUsers` entry whose user still exists becomes an external-identity
  link (`LdapUid` as the subject, `ProfileImageHash` as the avatar hash); an entry for a
  deleted user is skipped. The plugin is then enabled when any user's
  `AuthenticationProviderId` is LDAP-Auth's (ignoring case), and otherwise stays off. The file
  is only read. A missing file is silent; one that cannot be read or parsed, or a write that
  fails, is a warning, and the next boot tries again (`config.json` is written last, so its
  presence means the import finished). `scripts/migrate-jellyfin.sh` copies this file of
  Jellyfin's `plugins/`, and the `Jellyfin.Plugin.*.xml` settings of the metadata plugins
  Jellyfin builds in, which adoption imports the same way.
- Profile images (v24, `d11c86a`): the profile-image block of `Authenticate`,
  `LdapUtils.TryDetermineAttributeFormat`, `ProfileImageUpdater.SetProfileImage` and
  `LdapProfileImageSyncTask`. While `EnableLdapProfileImageSync` is on, `LocateLdapUser` also
  asks for `LdapProfileImageAttribute`, and a user the sign-in creates gets the first value of
  that attribute as their picture, stored as `profile.jpg` (`image/jpeg`, whatever the format,
  as upstream), its link recording the Base64 of the picture's MD5 as the avatar hash
  (`ProfileImageHash`). `LdapProfileImageFormat` reads the value: `Binary` as the bytes,
  `Base64` with `Convert.FromBase64String`'s rules (whitespace skipped, padding required, the
  unused bits of the last character ignored), `Url` by fetching it, and `Default` detects it on
  the value read as UTF-8: a control character other than tab, CR and LF, or DEL, is binary;
  else text `Base64.IsValid` accepts (which refuses non-zero unused bits before padding) is
  Base64; else an absolute `http` or `https` URL is a URL; anything else is upstream's
  `InvalidFormatException` message, not a guess. A returning user's sign-in leaves the picture
  alone, as upstream. The task (key `LdapProfileImageSync`, name `LDAP - Synchronize profile
  images`, description `Synchronizes user profile images from LDAP.`, category Application,
  every 24 hours, task id from `Jellyfin.Plugin.LDAP_Auth.LdapProfileImageSyncTask`) does
  nothing while the plugin is off, names no server, or has sync off. Otherwise, for each LDAP
  link it finds the entry by the link's uid through the user search filter (`LocateLdapUser`
  with the uid as the name); a user it does not find is a warning. With a picture there, an
  unchanged one (same hash, a picture stored) is skipped, and otherwise the old picture is
  cleared and the new one stored with its hash. Without one, `RemoveImagesNotInLdap` clears
  the user's picture and the hash.
- Admin filter (v24, `d11c86a`): the admin block of `Authenticate`. While `LdapAdminFilter` is
  neither empty nor `_disabled_` (ordinal), after the password is checked the bind DN searches,
  on a connection of its own, the subtree of `LdapAdminBaseDn` (the base DN when that is empty)
  with the filter's `{username}` replaced, ignoring case, by the entry's username attribute
  escaped as `SanitizeFilter` does. With `EnableLdapAdminFilterMemberUid` any entry found makes
  the user an administrator; otherwise one of them must be the user's entry (DNs compared
  ordinally). A new user gets the flag at creation (`false` while the filter is off, as upstream
  sets it); a returning user's flag follows it at each sign-in, and is left alone while the
  filter is off. The provider reports the flag in `ProviderAuthenticationResult.administrator`
  and the user manager applies it, because the login lock it holds does not nest.
- `LdapController.TestLdapFilters` with `GetFilteredUsers` (v24, `d11c86a`): the body
  (`UserFilterInfo`) is saved into the configuration first, then the bind DN counts the distinct
  DNs `LdapSearchFilter` finds in the base DN's subtree and, while the admin filter is on, those
  `LdapAdminFilter` finds, answering `LdapFilterResponse` (`Users`, `Admins`, `IsSubset`). As
  upstream, both filters are used as written (no `{username}` replaced) and both search the base
  DN, not `LdapAdminBaseDn`; an empty filter is Novell's `(objectclass=*)`. A directory that
  cannot be reached or refuses the bind DN is a `401` with `Failed to Connect or Bind to
  server.`; a search it fails, a filter that does not parse included, is a `400` with `User
  Filter: ` or `Admin Filter: ` before the reason, each as `LdapTestErrorResponse`
  (`{"Message": …}`).
- `LdapController.LdapUserSearch` with `LocateLdapUser` (v24, `d11c86a`): the body
  (`UserSearchAttributes`) saves `LdapSearchAttributes`, then, for a `TestSearchUsername` that
  is not empty, answers `UserSearchResponse` with the `LocatedDn` of the entry the name signs in
  as (the sign-in's own search); with no name, `{}`. A directory that cannot be reached or
  refuses the bind DN, a search it fails, and no matching entry are each a `401` with upstream's
  message (`Failed to Connect or Bind to server.`, `Error completing LDAP login while applying
  user filter.`, `Found no LDAP users matching provided username.`).
- Referrals (v24, `d11c86a`): `GetSearchConstraints` with `LdapAuthHandler`, on the three
  searches upstream sets them on: `LocateLdapUser`'s (sign-in, the password change and the user
  search), `GetFilteredUsers` (the filter test) and the admin filter's search at sign-in. A search
  reference and a `referral` result are followed alike, as the configured bind DN with its
  password, to the first of their URLs that connects and takes that bind (the URLs are
  alternatives, as Novell's `GetReferralConnection` tries them); the search there takes the
  URL's DN (else keeps its base), continues a one-level search at the base, and keeps its
  attributes, as Novell's `RebuildRequest` does. The entries found there join the search's,
  after those of the server that referred, and a referral a referred server sends is followed
  the same way. A referral none of whose URLs leads to a server that takes the bind (the name
  does not resolve, nothing answers, the TLS handshake or StartTLS fails, the bind DN is
  refused there, or the URL does not parse) is skipped (owner decision, gl-1207): the entries
  the other servers found stand, and the search logs one warning naming each server skipped
  and why. A user only a skipped referral holds is therefore not found, and signs in as no
  matching entry does: a `401`, counted toward lockout when the name is an account's (D4,
  below), as upstream counts it. A referred server that takes the bind and fails the search, a
  referral past the hop limit, and one past the budget of 10 referrals per search fail the
  whole search, naming that server (`host:port`): a server error at sign-in and in a password
  change, a `400` in the filter test, a `401` in the user search, as any failed search is. So
  does a referral that names no URL, which RFC 4511 does not allow (Novell's client throws an
  `IndexOutOfRangeException` there). The connection test's base search, the user's own bind
  (`ConnectToLdap(userDn, password)`) and the password change's write follow no referral and
  stay on the configured server, as upstream's set no search constraints: a password change for
  a user only a referred server holds is written to the configured server, which refuses it (a
  `500`) unless it holds the entry or chains the write, as upstream's.
- `POST /Ldap/TestServerBind`, `/Ldap/TestLdapFilters` and `/Ldap/LdapUserSearch` are the
  plugin's own controller, not contract operations, so they are in `handlers::PLUGIN_ROUTES`
  (matched ignoring case, as the page calls `ldap/TestServerBind` and the others so) and have no
  `handlers::VERIFIED` row; this section is their record. All three are administrators only
  (`RequiresElevation`), answer `404` while the plugin is disabled, and refuse a body that does
  not bind with ASP.NET's validation problem.
- Verified live (gl-1150, 2026-11-03), on a running server against OpenLDAP 2.6.8 in two
  containers from `contrib/ldap-test-directory/` (a directory and a partners directory it
  refers searches to): the connection test over plain LDAP, LDAPS and StartTLS with a custom
  root CA; sign-in through jellyfin-web 12.4's login form and through
  `jellyfin-apiclient-python`, the library Jellyfin MPV Shim signs in with; the settings
  page's three tests pressed in jellyfin-web; creation on and off; a rename and a uid change in
  the directory; the admin filter with the last-administrator rule; a password change by the
  RFC 3062 operation, a blank one refused; the reset URL; pictures at creation and from the
  task; a referral followed in all three modes, with no bind sent before TLS; an empty password
  with no connection made; the plugin disabled; and a directory paused, unroutable and closed.
  Adoption was checked on copies of the data directory of a stock Jellyfin 12.4 that had installed
  LDAP-Auth v24 from the official repository and created its LDAP users: they signed in to the
  same accounts, the plugin stayed off for one with no LDAP users, and a malformed
  `LDAP-Auth.xml` was a warning until it was corrected. Client certificates and the
  `memberUid` admin-filter mode are covered by the `ldap_directory` test, not by this pass.
  **Not tested:** Active Directory (and with it a forest's referrals, the `memberOf` filters AD
  setups use, and AD's own password-change rules), and a native client app.
- Accepted divergences (do NOT "fix" during sync):
  - **An empty password is refused before any connection.** Novell's client turns it into an
    anonymous bind, which upstream then refuses as not `Bound` (kept too, at the user's bind);
    `ldap3` would send it, and a directory may accept an unauthenticated bind (RFC 4513
    section 5.1.2), which would sign anyone in.
  - **Only the directory refusing the user's own bind counts as a failed sign-in** (owner
    decision D4: a directory outage is not a wrong password, and must not lock users out of
    their accounts), with no matching entry and creation turned off: each is a `401`, which the
    user manager counts toward lockout when the name signed in with is an account's (creation
    turned off only arises for a name that is not). The user's bind counts as refused only for
    the result codes that refuse credentials: `constraintViolation` (19, a password policy's
    lockout), `inappropriateAuthentication` (48), `invalidCredentials` (49),
    `insufficientAccessRights` (50) and `unwillingToPerform` (53). A directory that cannot be
    reached, does not answer within the D6 timeouts, fails the user's bind with any other code
    (`busy`, `unavailable`, `other`, …), refuses the configured bind DN, or fails the user
    search or the admin filter's search (a referred server failing it, a referral loop, the
    referral budget and a referral naming no URL included), is a `500` that is not counted. A
    local account signs in meanwhile, and an LDAP sign-in with an account's name fails after its
    own timeout without waiting for other accounts' sign-ins. Sign-ins with a name that is no
    account's (a uid or a mail, or a first sign-in) take turns, as upstream's do
    (`AuthenticateUser` locks `user?.Id ?? Guid.Empty`): during an outage each waits for those
    before it, so the second of two waits about twice the timeout (19.8 s measured with the
    directory paused). Upstream sets no timeout, so there they wait for as long as the directory
    does not answer. Whether to key that lock differently is an open owner decision (gl-1223).
    Upstream answers each of those failures with an `AuthenticationException`, a counted `401`
    (`Error completing LDAP login while applying admin filter.` for the admin filter's search).
    Ferrofin's login backoff still counts such an attempt for pacing, as it counts every attempt
    that does not succeed.
  - **The admin filter never demotes the last administrator** (owner decision D5: there must
    always be at least one administrator): removed from the directory's administrators while no
    other administrator exists, the user signs in, stays one, and the server logs a warning.
    With two or more the filter demotes as upstream does. The user manager reads the count in
    the transaction that writes the flag.
  - **An entry without the username attribute is a `400` while the admin filter is on**, as it
    is when the user is created or renamed. Upstream's `SanitizeFilter` throws a
    `NullReferenceException` on the null name first, a `500`.
  - **The filter test keeps the saved `EnableLdapAdminFilterMemberUid` when the body leaves it
    out** (a Jellyfin bug not ported). Upstream saves the missing member as `false`, and its own
    settings page never sends it, so pressing "Test filters" there turns member-uid mode off
    until the page is saved again, and the admin filter then demotes the administrators it found
    through a group. A body that sends it still sets it, and a `null` does not bind, as a C#
    `bool` refuses it.
  - **A filter the directory client cannot parse** is reported as `User Filter: Filter Error`
    (or `Admin Filter: …`): `ldap3` gives no reason. Upstream pulls Novell's parser message out of
    the exception text with a regex.
  - **A blank new password is refused** (owner decision D11) with a `400`, before any
    connection, so the directory is not touched. That is what a reset (`ResetPassword: true`)
    sends. Upstream passes it on: its `PasswordModifyOperation` then omits the new password, and
    RFC 3062 has the directory generate a random one that nobody learns. Blank means empty or
    white space, as the user manager reads it for administrators.
  - **A password change whose directory cannot be reached, or refuses the bind DN, is a `500`**,
    as at sign-in (D4). Upstream's `ConnectToLdap` throws `AuthenticationException`, a `401`.
    The connection is closed after the change; upstream's is never disposed.
  - **A password change writes only to the user's own entry.** The entry the user's name finds
    must belong to them (by its uid link, or by name for an account LDAP-Auth owns), as at
    sign-in; otherwise the change is a `401` and nothing is written. Upstream writes to whichever
    entry matched first, so a person who can make their own entry match another user's name (a
    `mail` they may edit) would have that user's new password set on their own entry. The match
    trusts the username and uid attributes, as sign-in does, so where users can write them a new
    password can still reach the wrong entry (kept as upstream by owner decision, gl-1188).
  - **A uid link to an account LDAP-Auth no longer owns is ignored** (gl-1230), as the lookup by
    name ignores such an account. Upstream's
    `GetUserById(GetLdapUser(ldapUid).LinkedJellyfinUserId)` follows the link whatever the account's
    `AuthenticationProviderId`, and Jellyfin's `AuthenticateUser` then rewrites that id to the
    provider that succeeded. So when an administrator moves a linked user to the default provider (a
    local break-glass administrator, say), or an adopted `LdapUsers` still lists such a user, a
    sign-in with another name for the entry (its uid or mail) that the directory accepts switches
    the account back to LDAP-Auth, and its local password stops working. Here the sign-in goes on as
    if no link existed: the lookup by name, then creation (which refuses a second account of the
    same name) or upstream's creation-off `401`. The link is kept, not removed, so moving the
    account back to LDAP-Auth finds it again, unless a sign-in has meanwhile linked the entry to
    another account (one it created, or an LDAP-Auth account named as the entry's username
    attribute), which takes the link over.
  - **No PIN is ever this provider's to redeem**: `RedeemPasswordResetPin` answers no match.
    Upstream throws `NotImplementedException`, which stops the redemption before a later
    provider could redeem its own.
  - **A followed referral never downgrades TLS.** Novell's client opens the referred connection
    with default options, plain LDAP with no StartTLS, and binds over it, so upstream sends the
    bind password in clear to a referred server even when it is configured for LDAPS or
    StartTLS. Here the referred connection uses what is configured: LDAPS stays LDAPS, StartTLS
    stays StartTLS, with the same certificate checks (against the referred server's name) and
    client certificate, and an `ldaps://` URL is LDAPS whatever is configured. A URL without a
    port takes the port of the protocol used, 636 for LDAPS and 389 otherwise (Novell: the
    scheme's). The bind DN and password still go to whichever server the directory names, as
    upstream sends them: a directory that can name any host in a referral can collect the bind
    password there, though with TLS configured (and `SkipSslVerify` off) that host must pass
    the certificate check first.
  - **A referral that leads to no server is skipped wherever it arrives, with a warning**
    (owner decision, gl-1207). Upstream gets this only by accident of Novell 3.6.0's search
    results: `LdapSearchResults.BatchOfResults` keeps the `LdapReferralException` a failed chase
    throws without counting it as an entry, so an iteration that has read every entry never sees
    it (`ResetVectors` drops it), one followed by another entry throws it at the next `Next()`,
    and a `referral` result that cannot be followed ends the results empty. Active Directory
    sends its search references after the entries, where the two agree: a domain-root base DN
    whose `ForestDnsZones`, `DomainDnsZones` and `Configuration` references do not resolve or
    do not answer still signs users in, while the search stays within the referral budget
    (below). With TLS configured, a referred server whose certificate is not valid for the name
    in the reference is skipped too; upstream connects to it in plain LDAP with no check (see "A
    followed referral never downgrades TLS"). The order of a search's entries and references is
    not kept here, so a reference before an entry is skipped too, and upstream logs nothing;
    here each search that skipped any logs one warning naming each server and why (a URL that
    does not parse by its reason, never quoted).
  - **A referral chain stops after 10 referrals in a row** (owner decision, Novell's default
    `HopLimit`): a referred server that refers the search on once more fails it, which is what
    stops a referral loop. Novell 3.6.0's search results restart the hop count at every
    referral they read (`LdapSearchResults` calls `ChaseReferral` with a count of 0), so there
    the limit never trips and a loop keeps opening connections.
  - **A search chases at most 10 referrals in all** (`REFERRAL_BUDGET`, a constant, not a
    setting; owner decision, gl-1208), whether each is followed or skipped: the hop limit bounds
    only a chain, an answer may carry any number of references, and every referral costs the D6
    timeouts again (5 s to connect and 10 s per operation, for each of its URLs). So this bounds
    how many referrals one search chases, and with them most of the time it can hold the
    unknown-name sign-in lock; a referral's URLs are not counted, and a sign-in with the admin
    filter on runs two searches, each with its own budget. The referral past it fails the search
    before it is chased: `the search was referred more than 10 times in all, the last time by
    <host:port>: no search follows more referrals than that`, a server error at sign-in, a `400`
    in the filter test and a `401` in the user search. A referral counts once, whatever its
    number of URLs. The hop limit is checked first, so a chain of 11 chased before any other
    referral still fails with the hop limit's error; once any other referral has been chased,
    skipped ones included, the budget runs out first. Novell 3.6.0 sets no total, so a search
    that meets more than 10 referrals works upstream and fails here: Active Directory sends one
    reference per child domain beside its `ForestDnsZones`, `DomainDnsZones` and
    `Configuration` references, so a base DN at the root of a domain with many child domains
    can pass the budget (the referrals the referred servers send count too). A base DN at the
    OU that holds the users, which has no subordinate references, avoids it.
  - **Referral URLs are read as RFC 4516 writes them**: the DN and the filter percent-decoded,
    and the URL's filter and scope used when it names them (RFC 4511 sections 4.1.10 and
    4.5.3). Novell sends the DN as written, ignores the URL's scope, and ignores a `referral`
    result's filter. A URL that is not `ldap://` or `ldaps://`, or does not parse, is skipped
    as an alternative that fails, as Novell skips one, and is never quoted in an error or a log
    (a URL extension can carry a password).
  - **A name that finds another account's entry is refused** (a counted `401`). When the name
    signed in with is a Ferrofin user's, the entry the search found must be that user's (by
    its uid link, or by name). Upstream signs in the user the name named with whichever entry
    came first, and creates or renames another account on the way, so a person who can make
    their own entry match another user's name (a `mail` or `displayName` they may edit) could
    sign in as that user with their own password. The entry's username attribute
    (`LdapUsernameAttribute`) is still trusted, as upstream trusts it: an entry with no uid link
    yet whose username attribute names an account LDAP-Auth owns is relinked to that account
    (upstream's relink after a uid change; see the next item for an account linked to a uid the
    directory still has), so that attribute must be one users cannot edit.
    In a directory where users can write their own username attribute (OpenLDAP's common
    `by self write`), a user with no account linked to their uid yet can set it to an
    LDAP-owned account's name and sign in to that account with their own password, whatever its
    rights (while the admin filter is on, the account's administrator flag then follows the
    filter for the signing-in user's own entry). When the account's own user next signs in, it
    is relinked back, so the two can keep taking it from each other, and a relink does not end a
    session already issued. The uid attribute (`LdapUidAttribute`) is trusted the same way, as
    upstream's `GetLdapUser` trusts it: a user who can write their own can set it to a linked
    user's uid and sign in to that account, which is then renamed to their own username
    attribute. Kept as upstream by owner decision (gl-1188, 2026-11-03: to be revisited if it is
    proven a security issue). The username-attribute route was then reproduced live against
    OpenLDAP, the directory administrator editing the entry standing in for a user; the uid route
    is read from the code, not run. Whether that triggers the revisit is the owner's call.
  - **An account is never relinked by name while its own uid is still in the directory**
    (owner decision, gl-1229). When the account an entry's username attribute names is already
    linked to another uid, the bind DN searches the base DN's subtree for
    `(<LdapUidAttribute>=<that uid>)`, following referrals, after the entry's own password
    binds. An entry other than the signing-in one means the name belongs to another person: the
    sign-in is a `401` (counted when the name signed in with is the account's) and the server
    logs a warning naming both entries; a password change for the account is a `401` the same
    way, and nothing is written. No entry means the uid really changed (upstream's relink
    case), and the entry is relinked; so is it when the search finds only the signing-in entry
    itself (its uid changed only in case), and when no `LdapUidAttribute` is configured, which
    leaves nothing to tell entries apart by. A directory that fails that search is a `500`, never
    a relink (D4). Upstream relinks whatever link the account has, so two entries sharing a
    username attribute (with upstream's default `cn`, two people of the same name) take one
    account from each other at each sign-in, with its libraries, history and, while the admin
    filter is on, administrator flag. An account with no link at all is still relinked by name
    as upstream does, so for accounts not linked yet the username attribute must be unique as
    well as one users cannot edit.
  - **A relink by name is written only after the password is checked.** Upstream saves it
    before it binds as the user, so a wrong password still writes it.
  - **An entry without the uid attribute is not linked.** Upstream stores it with a null
    `LdapUid`, which no lookup finds, and in doing so drops that user's earlier link and every
    other null-uid link; its profile-image sync then fails on the null uid. Such a user is
    found by name at each sign-in, and keeps the link of the uid they had.
  - **A new user is created with its provider ids and folder access in one transaction**
    (`UserManager::create_external_user`). Upstream's `CreateUserAsync` commits a local,
    passwordless user first and sets the provider ids after, a moment in which the built-in
    provider signs that user in with an empty password. The "User … has been created"
    activity entry is recorded as `CreateUserAsync` records it.
  - **Off by default** (owner decision D3): the plugin is disabled in `/Plugins` until an
    administrator enables it, and `LdapServer` seeds empty instead of
    `ldap-server.contoso.com`. Upstream's provider is always enabled and would try the
    placeholder host on every sign-in of an unknown name. An adopted `LDAP-Auth.xml` whose
    `LdapServer` is still that placeholder is imported with the server empty, and a warning
    names `LdapServer`: the plugin is on for its LDAP users but inactive until an
    administrator sets a server.
  - **No `LdapUsers` in the configuration** (D12: one store of external identities, which the
    OIDC sign-in planned next is to share, that links an identity to at most one user, writes
    each link in a transaction so concurrent first sign-ins cannot lose one, and removes a
    user's links with the user): which LDAP uid belongs to which user lives in Ferrofin's
    external-identity table, so `GET /Plugins/{id}/Configuration` does not return it, and a
    `LdapUsers` member posted to it is dropped. Upstream's settings page never reads or writes
    it.
  - **An adopted `LDAP-Auth.xml` is imported once and never written back.** Upstream reads and
    writes that file as the plugin's live store; Ferrofin keeps `config.json` and the links
    table from the import on, so a change made in Ferrofin never reaches the file, which stays
    as Jellyfin last wrote it. The importer trims each value, as it does `system.xml`'s, so a
    setting with leading or trailing spaces (a bind password, say) comes across without them.
  - **Timeouts** (D6): opening a connection (the TCP connect and an LDAPS handshake) gives up
    after 5 seconds, and each operation, StartTLS included, after 10 seconds without an
    answer (a search's timer restarts with each entry). Upstream sets none, so a directory that
    stops answering holds a sign-in, and every unknown-name sign-in queued behind it, for as long
    as it does not answer.
  - **A custom root CA is the only trust anchor**: the server's certificate must chain to a
    certificate in that file, or be one of the file's certificates itself (a self-signed
    server certificate given as the root CA, which webpki alone refuses when it is marked as a
    CA, as `openssl req -x509` marks it), and it must be valid for the server's name and
    within its validity period. Upstream also rejects a name mismatch, but its chain policy
    sets `AllowUnknownCertificateAuthority`, with which .NET accepts a chain ending at a root
    that is not in the file. A root CA file with a certificate rustls cannot use as a root
    skips it, and fails only when none is usable.
  - **Unreadable certificate files** fail the test's Connect step with the reason. Upstream
    reads the client certificate before its `try`, so the request fails with a `500`; a client
    certificate is read here only for a TLS connection, where it is used.
  - **Error wording**: `Error` is the RFC 4511 result name (spelled as Novell's client does,
    `Invalid Credentials`) with the server's diagnostic, or the transport's own message
    (rustls' certificate errors, `could not connect to host:port within 5 seconds`, `the
    directory closed the connection: …`). Upstream reports the Novell exception's message.
  - **The base search asks for no attributes** (`1.1`), and neither do the filter test's
    searches or the admin filter's at sign-in: they read only DNs, the same either way, without
    the directory sending every entry's attributes. A search reference fails the connection
    test's base search with Novell's `Search result reference received, and referral following
    is off`, as upstream's iterator throws on one there.
  - **Certificate checks are webpki's** beyond the name and the trust anchor: X.509 v3
    certificates only, no SHA-1 signatures in a chain, the `serverAuth` extended key usage
    when the certificate lists any, and no revocation check (.NET checks revocation online,
    ignoring an unknown status).
  - **The common-name fallback is Ferrofin's own, and narrower**: an exact match ignoring
    case, with no wildcard, for a host name only (never for an IP address), and off
    whenever any root in the root CA file or the system store, or any intermediate the
    server presents, carries name constraints, since webpki checks those against
    alternative names only. A legacy certificate naming its host only as a wildcard or an
    address in the common name may connect upstream and is refused here; both fail closed.
  - **A stored `LdapProfileImageFormat` that names no format** reads as `Default`, so one bad
    value cannot make the configuration unreadable. .NET's deserializer fails the whole
    configuration and the plugin starts from its defaults.
  - **A picture given as a URL is fetched under the server's image download limits**, through
    the `RemoteImageFetcher` seam over the providers' download path: a private address is
    refused, on the request and on every redirect, unless `image_private_http_allow` lists it
    (gl-1096); the body is read under `image_download_max_mb` (gl-1095); and it must start
    like an image. Upstream fetches it with the default `HttpClient`, so a directory entry
    could make the server read an internal service. A refused or failed fetch is a warning.
  - **A picture that cannot be read is a warning for that user, never a failure.** Detection
    that fails, Base64 that does not decode or a download that fails leaves a new user without
    a picture (the sign-in succeeds; at sign-in a picture or hash that cannot be stored is a
    warning too) and leaves the task's user as they were while it goes on to the next.
    Upstream's exceptions fail the sign-in of the user it has just created, before it links
    them, or stop the whole task. A search the directory fails for one user is a warning and
    the next user is synced, as upstream catches that `AuthenticationException`; a user's
    picture or link that cannot be stored fails the task, as upstream's.
  - **A new user is linked before their picture is read**, and the picture's hash is recorded
    once it is stored, so a concurrent sign-in finds the account by its uid while a URL picture
    downloads. Upstream links after the picture. The download still runs inside the sign-in, as
    upstream's does, bounded by the image download limits (the provider timeout and the origin's
    pacing); a slow picture host therefore delays the first sign-ins that wait on the same
    login lock.
  - **The task fails when the directory cannot be reached or refuses the bind DN**, naming the
    step, at the first user. Upstream catches that `AuthenticationException` for each user and
    goes on, so every user would wait out the connect timeout (D6) in turn and the run would
    report completed with nothing synced.
  - **An entry without the picture attribute is logged at debug**: most users have none, and
    the task would warn about each of them every day. Upstream's `GetAttribute` warns.
  - **The task skips a link whose user is gone and a link with an empty uid.** Links go with
    their user, so only a deletion racing the task leaves the first; upstream's
    `NullReferenceException` stops the task there. An empty uid finds nothing, so it is not
    searched for; upstream searches and warns, as the task does here.
  - **The sync asks for the picture attribute only when it is named**: an empty
    `LdapProfileImageAttribute` is not sent in the search's attribute list, since no entry has
    an attribute without a name. Upstream sends the empty name.
  - **An error's value is redacted**: a detection error names a URL by its scheme and host
    (`redact_url`, which leaves a `file:` URL whole) and any other value by its length only,
    and so does a failed fetch (the `Url` format set for an attribute that holds no URL
    included), so a web URL's path and credentials, or an attribute that is not the picture (a
    password hash, a whole encoded image), stay out of the log. A detection error still names
    the scheme of a value that has one, as upstream's does. Upstream's message quotes the
    value whole.
