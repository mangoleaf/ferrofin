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
| LDAP-Auth | https://github.com/jellyfin/jellyfin-plugin-ldapauth | `d11c86a` | 24 | in progress: plugin, settings page and `TestServerBind` (milestone "LDAP-Auth plugin port") |

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
    names the Chromaprint backend.
  - **Task keys:** the detection task was `IntroSkipper.Detect` before D6; the
    task manager adopts what was saved under that key.
- **Memory** (implementation only, the behaviour is the same): upstream
  holds each library's list of episodes and movies while it queues them. The
  queue here takes a snapshot of the ids its query matches and reads the rows
  by id, `PAGE_SIZE` (100, upstream's `QueryPageLimit`) at a time: first to
  place each item in its library, then library by library to queue it. The
  items, their libraries and their order are the same, so the queue is the
  same. Rows that tie on all four sort keys keep whatever order SQLite's plan
  gives them; neither the snapshot nor the whole list promises one.

### File Transformation
- Ferrofin files: `crates/ferrofin-extensions/src/file_transformation.rs`,
  vendored assets `crates/ferrofin-extensions/assets/filetransformation/`.
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
- Divergence PENDING OWNER DECISION (D10 in
  `brain/plans/PLAN_ITEM_FILE_DELETION.md`): the pinned plugin (`e6f58d6`,
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
  name with no such key keeps upstream's key. The self-heal also undoes a
  group made by hand through `POST /Videos/MergeVersions` of two airings, or
  of two copies only one of whose file names carries the key; the playlist
  and collection entries that merge rerouted to the primary stay on it.
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
  configuration, the `LdapDirectory` seam, `TestServerBind` and its service) and
  `ldap_auth/directory.rs` (the `ldap3` client and its rustls configuration), vendored
  assets `crates/ferrofin-extensions/assets/ldapauth/`, the trait seam
  `crates/ferrofin-traits/src/ldap.rs`, the wire types `crates/ferrofin-model/src/ldap.rs`,
  and the thin HTTP handler `crates/ferrofin-api/src/handlers/ldap.rs`. The live directory
  test is `crates/ferrofin-extensions/tests/ldap_directory.rs` (`FERROFIN_LDAP_TESTS=1`,
  OpenLDAP in docker; CI does not run it).
- Ported so far (v24, `d11c86a`): `LdapPlugin` (GUID, name `LDAP-Auth`, version 24,
  `ConfigurationFileName` `LDAP-Auth.xml`, the settings page under its name),
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
  Linux. Sign-in, the filter and user-search tests, password changes, profile images,
  referrals and the `LDAP-Auth.xml` import are later phases of the milestone.
- `POST /Ldap/TestServerBind` is the plugin's own controller, not a contract operation, so it
  is in `handlers::PLUGIN_ROUTES` (matched ignoring case, as the page calls
  `ldap/TestServerBind`) and has no `handlers::VERIFIED` row; this section is its record.
- Accepted divergences (do NOT "fix" during sync):
  - **Off by default** (owner decision D3): the plugin is disabled in `/Plugins` until an
    administrator enables it, and `LdapServer` seeds empty instead of
    `ldap-server.contoso.com`. Upstream's provider is always enabled and would try the
    placeholder host on every sign-in of an unknown name.
  - **No `LdapUsers` in the configuration** (D12): which LDAP uid belongs to which user lives in
    Ferrofin's external-identity table, so `GET /Plugins/{id}/Configuration` does not return
    it, and a `LdapUsers` member posted to it is dropped. Upstream's settings page never reads
    or writes it.
  - **Timeouts** (D6): opening a connection (the TCP connect and an LDAPS handshake) gives up
    after 5 seconds, and each operation, StartTLS included, after 10 seconds without an
    answer (a search's timer restarts with each entry). Upstream sets none.
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
  - **The base search asks for no attributes** (`1.1`): the count it reports is the same,
    without the directory sending every entry's attributes. A search reference fails the step
    with Novell's `Search result reference received, and referral following is off`, as
    upstream's iterator throws on one.
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
