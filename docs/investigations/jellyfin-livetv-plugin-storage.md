# How Jellyfin stores Live TV and plugin settings, and what an export can carry

The planned `ferrofin-server export-jellyfin` command copies a stopped Ferrofin data directory into one Jellyfin
can start on, and lists what it cannot carry in a loss report (gl-1548, gl-1553). This file is the evidence for two
of that feature's open decisions (gl-1571):

- **D6, Live TV:**
  - A: write tuners and guides as `livetv.xml`, and timers and recordings too where Jellyfin's storage maps safely;
  - B: write timers and recordings whatever it takes;
  - C: report all Live TV as lost.
- **D7, plugin settings:**
  - A: write each plugin's current settings as the file a reinstalled Jellyfin plugin reads, and list them in the
    loss report;
  - B: report them as lost.

Two other names recur: **D5** is the decision on the export's output layout and how it copies the large trees
(artwork, trickplay, subtitles; this file adds the recording folders to it), and **the gate** is the round-trip gate that boots real Jellyfin on an
export (gl-1546).

Upstream paths are in `jellyfin/jellyfin` at `v12.4` unless another repository is named. Ferrofin paths are at
`release/v1.6.0` (`1893be47d`).

## Summary

| Question | Answer | Evidence |
|---|---|---|
| Where Jellyfin keeps tuners and guides | `config/livetv.xml`, `<TunerHosts>` and `<ListingProviders>` | C#, reproduced |
| Where Jellyfin keeps timers and series timers | `data/livetv/timers.json` and `data/livetv/seriestimers.json`, not the database | C#, reproduced |
| Where Jellyfin keeps recordings | media files and NFO sidecars in the recording folders, scanned as libraries; a `Completed` entry stays in `timers.json` | C# |
| Where Jellyfin keeps plugin settings | `plugins/configurations/<name>.xml` | C#, reproduced for the bundled plugins |
| Do Ferrofin's timers map to those files without loss | Yes for scheduled timers and series timers: Ferrofin's stored rows, mapped by upstream's own DTO mapping plus two Ferrofin columns, came back from Jellyfin's guide refresh identical to what Jellyfin writes itself. Proven for an M3U tuner with a UTC XMLTV guide only, and only if the first guide refresh succeeds. | reproduced on Jellyfin 12.4 |
| Do Ferrofin's plugin settings map to those files | LDAP-Auth: yes, every setting, with its uid map from `FerrofinExternalIdentities`. The other plugins: not checked field by field, and two known shape differences | code and live |
| D6 | **A**, including timers, series timers and the recordings' completed timers | below |
| D7 | **A**, for LDAP-Auth (uid map included) and for every plugin whose settings match the upstream shape | below |

## Jellyfin's storage

### Tuners and guides: `config/livetv.xml`

- The `livetv` named configuration (`Jellyfin.LiveTv/Configuration/LiveTvConfigurationFactory.cs:20`) is
  `LiveTvOptions`. It holds the scalar options, the recording paths, `TunerHosts`, `ListingProviders` and
  `MediaLocationsCreated` (`MediaBrowser.Model/LiveTv/LiveTvOptions.cs:18-46`). It is serialized as
  `{config}/livetv.xml` like every named configuration
  (`Emby.Server.Implementations/AppBase/BaseConfigurationManager.cs:273`). Adding or removing a tuner or a guide saves the whole document
  (`Jellyfin.LiveTv/TunerHosts/TunerHostManager.cs:93`, `:108`; `Jellyfin.LiveTv/Listings/ListingsManager.cs:77`,
  `:93`, `:261`).
- Reproduced: after `POST /LiveTv/TunerHosts` and `POST /LiveTv/ListingProviders` on a scratch 12.4, the only new
  Live TV configuration file was `config/livetv.xml`, with both lists inside (`TunerHostInfo` and
  `ListingsProviderInfo` elements).

### Timers and series timers: `data/livetv/*.json`

- `TimerManager` stores `TimerInfo` in `{DataPath}/livetv/timers.json`
  (`Jellyfin.LiveTv/Timers/TimerManager.cs:25`), and `SeriesTimerManager` stores `SeriesTimerInfo` in
  `{DataPath}/livetv/seriestimers.json` (`Jellyfin.LiveTv/Timers/SeriesTimerManager.cs:16`). `DataPath` is
  `{program data}/data` (`Emby.Server.Implementations/AppBase/BaseApplicationPaths.cs:36`), `/config/data` in the
  docker image.
- Each file is a JSON array written whole on every change (`Jellyfin.LiveTv/Timers/ItemDataProvider.cs:70-75`)
  with `JsonDefaults.Options` (`ItemDataProvider.cs:20`): property names as declared (PascalCase), enums as
  strings, nulls left out (`Jellyfin.Extensions/Json/JsonDefaults.cs:29-48`).
- The ids in those files are the **external** ids:
  - `TimerInfo.Id`: the timer's own id;
  - `ChannelId`: the tuner's channel id (`m3u_…`);
  - `ProgramId`: the listings programme id (`{tvg-id}_{start:O}_{channel id}` for XMLTV);
  - `SeriesTimerId`: the series timer's own id.

  The API's `TimerInfoDto` carries the internal GUIDs in `Id`/`ChannelId`/`ProgramId`/`SeriesTimerId` and the
  external ones in `ExternalId`/`ExternalChannelId`/`ExternalProgramId`/`ExternalSeriesTimerId`.
  `LiveTvDtoService.GetTimerInfo` and `GetSeriesTimerInfo` (`Jellyfin.LiveTv/LiveTvDtoService.cs:431-545`) are
  upstream's mapping from the DTO to the file.
- A completed recording keeps its timer, `Status = Completed` with `RecordingPath`
  (`Jellyfin.LiveTv/Recordings/RecordingsManager.cs:414-419`). A capture that left no file deletes the timer
  (`:421-424`).
  - `GetTimersAsync` hides completed timers from the API (`Jellyfin.LiveTv/DefaultLiveTvService.cs:392-400`), so
    the guide refresh's `RefreshTimers` never refreshes them.
  - Jellyfin still reads them:
    - the series "keep up to" pruning reads `SeriesTimerId`, `Status`, `RecordingPath` and `EndDate`
      (`RecordingsManager.cs:683-688`);
    - recording a programme again finds its completed timer by `ProgramId` and revives it as a manual timer
      (`DefaultLiveTvService.cs:219-229`);
    - a series fan-out finds one by `Id` or `ProgramId`, refreshes its metadata and sets it back to `New` unless it
      `IsManual` (`:716-757`), while the programme is still in the guide;
    - cancelling a series timer matches by `SeriesTimerId` (`:149-157`).

    So a completed timer needs its `Id`, `ProgramId`, `ChannelId`, `SeriesTimerId` and `IsManual` as well.
- Nothing about timers is in the database. Reproduced: one manual timer and a series timer (six airings) gave
  7 entries in `timers.json` and 1 in `seriestimers.json`, and no database table holds them.

### Channels and programmes: database items, rebuilt from the guide

- Jellyfin keeps channels and programmes as `BaseItems` rows (`LiveTvChannel`, `LiveTvProgram`; the scratch
  server had 1 and 7). Their GUIDs are derived from the external ids
  (`LiveTvDtoService.cs:403-408`, `:424-429`), and a guide refresh rebuilds them.
- After a guide refresh `GuideManager` runs `RefreshSeriesTimers` and `RefreshTimers`
  (`Jellyfin.LiveTv/Guide/GuideManager.cs:163-164`):
  - `RefreshTimers` (`DefaultLiveTvService.cs:88-116`) deletes a timer that has ended and deletes one whose
    programme the guide no longer has (`:107-112`). On the rest it runs `CopyProgramInfoToTimerInfo` (`:895`),
    which copies the season and episode, `SeriesId`, provider ids, tags and flags.
  - `RefreshSeriesTimers` re-derives each series timer's airings. On an airing timer that already exists it
    overwrites about twenty fields from the programme, `ShowId` and `Genres` among them, which `RefreshTimers` does
    not copy (`UpdateExistingTimerWithNewMetadata`, `:365-390`, called at `:742`).
- A series timer finds its airings by `ExternalSeriesId = SeriesId`, and by name only when `SeriesId` is empty
  (`DefaultLiveTvService.cs:806-823`). The timer it makes for an airing has the id
  `(seriesTimer.Id + programme.ExternalId).GetMD5()` (`:867`), so a timer already on file is found again, not
  scheduled twice.

### Recordings

- Captures go to `{DataPath}/livetv/recordings` unless `RecordingPath` says otherwise
  (`RecordingsManager.cs:106-116`).
- Each recording folder becomes a library (`GetRecordingFolders`, `:143`; `CreateRecordingFolders` calls
  `AddVirtualFolder`, `:213`), so a recording is an ordinary scanned item with its NFO. The recordings view lists
  the items under those folders (`Jellyfin.LiveTv/LiveTvManager.cs:433-441`).
- `MediaLocationsCreated` lists the libraries made that way. At every start, `CreateRecordingFolders` removes the
  library for each listed location that is no longer a recording folder (`RecordingsManager.cs:224-240`, then
  `RemovePathFromLibraryAsync`, `:248`).

### Plugin settings

- A plugin's settings file is `{PluginsPath}/configurations/<ConfigurationFileName>`
  (`MediaBrowser.Common/Plugins/BasePluginOfT.cs:131`;
  `Emby.Server.Implementations/AppBase/BaseApplicationPaths.cs:61`), written by `XmlSerializer`. The file name
  defaults to the assembly name with `.xml` (`BasePluginOfT.cs:125`).
- The bundled metadata plugins override the file name:
  - `Jellyfin.Plugin.Tmdb.xml` (`MediaBrowser.Providers/Plugins/Tmdb/Plugin.cs:46`);
  - `Jellyfin.Plugin.Omdb.xml` (`Omdb/Plugin.cs:31`);
  - `Jellyfin.Plugin.MusicBrainz.xml` (`MusicBrainz/Plugin.cs:68`);
  - `Jellyfin.Plugin.AudioDb.xml` (`AudioDb/Plugin.cs:31`);
  - `Jellyfin.Plugin.StudioImages.xml` (`StudioImages/Plugin.cs:52`).

  Reproduced: the scratch 12.4 wrote `plugins/configurations/Jellyfin.Plugin.Tmdb.xml` and `…MusicBrainz.xml` at
  first start. 12.4 also bundles ListenBrainz, which Ferrofin does not provide (gl-220).
- The plugins Ferrofin has an extension for keep the default name. None of their projects sets `<AssemblyName>`,
  so the project name is the assembly name:

  | File | Project |
  |---|---|
  | `LDAP-Auth.xml` | `jellyfin-plugin-ldapauth` `v24`, `LDAP-Auth/LDAP-Auth.csproj` |
  | `IntroSkipper.xml` | `intro-skipper` `db09359`, `IntroSkipper/IntroSkipper.csproj` |
  | `Jellyfin.Plugin.MergeVersions.xml` | `jellyfin-plugin-mergeversions` `12.0.0`, `Jellyfin.Plugin.MergeVersions/Jellyfin.Plugin.MergeVersions.csproj` |
  | `Jellyfin.Plugin.FileTransformation.xml` | `jellyfin-plugin-file-transformation` `f4f01c3`, `src/Jellyfin.Plugin.FileTransformation/Jellyfin.Plugin.FileTransformation.csproj` |
  | `Jellyfin.Plugin.OpenSubtitles.xml` | `jellyfin-plugin-opensubtitles` `23d00cb`, `Jellyfin.Plugin.OpenSubtitles/Jellyfin.Plugin.OpenSubtitles.csproj` |
- LDAP-Auth v24 (`jellyfin-plugin-ldapauth`, `d11c86a`) keeps its uid map in the same file
  (`LDAP-Auth/Config/PluginConfiguration.cs:53`, `LdapUsers`). Sign-in finds the user by uid first
  (`LDAP-Auth/LDAPAuthenticationProviderPlugin.cs:100`). It falls back to the user name, and only for a user whose
  provider is LDAP-Auth's (`:108-129`), so a user whose directory name changed becomes a new user when the map is
  missing. Its certificate settings are host paths (`PluginConfiguration.cs:123`, `:128`, `:133`).
- Without the plugin, a user whose `AuthenticationProviderId` names it is given the invalid provider and cannot
  sign in (`Jellyfin.Server.Implementations/Users/UserManager.cs:988-1004`) until the plugin is installed again.

## Ferrofin's storage

### Live TV

- Tuners and listing providers are database rows (`FerrofinLiveTvTunerHosts`, `FerrofinLiveTvListingProviders`)
  whose `Data` column is the whole upstream DTO (`crates/ferrofin-core/src/live_tv_import.rs:72-105`; the import
  test checks the DTO round-trips).
- The scalar options are `named/livetv.json`, and the served document merges the rows back in. Adoption seeds that
  document without the three recording paths (`crates/ferrofin-core/src/config_import.rs:84-96`). An admin can set
  them later in Ferrofin (`crates/ferrofin-livetv/src/dvr.rs:650-657` reads `RecordingPath`).
- A timer row's `Id` is the internal DTO id, and `Data` is the whole `TimerInfoDto`, external ids included
  (`crates/ferrofin-livetv/src/dvr_repository.rs:50-90`). `ExternalChannelId` is filled from the channel at
  `crates/ferrofin-livetv/src/manager.rs:4196` for a timer and `:4361` for a series timer. `IsManual` is its own
  column.
- A timer whose recording finishes is kept with `Status = Completed` (`manager.rs:6502-6503`), as upstream keeps it
  in `timers.json`. A series timer row adds `ExternalId` and `SeriesId` columns (`dvr_repository.rs:135`).
- A recording row (`FerrofinLiveTvRecordings`) is Ferrofin's own bookkeeping, with no Jellyfin table to go to
  (`dvr_repository.rs:253-300`):
  - it names its timer by the internal id and its file by `Path`;
  - it keeps the programme facts a completed timer needs: `EpisodeTitle`, `SeasonNumber`, `EpisodeNumber`,
    `ProductionYear` and the `Is*` flags.
- The recording folders are libraries as upstream's are (`crates/ferrofin-livetv/src/recording_folders.rs:1-21`),
  and the default folder is the same `{data}/livetv/recordings` (`dvr.rs:650-657`).
- Channel and programme GUIDs are derived as upstream derives them (`manager.rs:261-284`):
  - the XMLTV programme id has the same `{channel}_{start:O}` form, with the channel id appended
    (`manager.rs:1006-1022`);
  - a series timer's airing ids follow the same MD5 rule (`manager.rs:7619-7650`).

  Channels and programmes live in `FerrofinLiveTvChannels` and `FerrofinLiveTvPrograms`, not in `BaseItems`.

### Plugin settings

- Every plugin's settings are `{config}/plugins/<plugin id>/config.json`
  (`crates/ferrofin-core/src/plugin_manager.rs:647-648`; the directory is chosen at
  `apps/ferrofin-server/src/state.rs:2049-2053`). Ferrofin writes no Jellyfin XML.
- LDAP-Auth's `config.json` holds every upstream `PluginConfiguration` member except `LdapUsers`. Checked live: it
  has 28 of upstream's 29 members.
  - The uid map is in `FerrofinExternalIdentities` (`crates/ferrofin-db/migrations/0048_external_identities.sql`).
  - Its rows are under the provider id `Jellyfin.Plugin.LDAP_Auth.LdapAuthenticationProviderPlugin`
    (`crates/ferrofin-extensions/src/ldap_auth/provider.rs:140`).
  - `Subject` holds the uid and `AvatarHash` the profile image hash.
- The other plugins' settings were not compared field by field. Two differ from upstream already:
  - OpenSubtitles stores `ApiKey`, `Username` and `Password` (`apps/ferrofin-server/src/state.rs:1936`). Upstream
    has `Username`, `Password` and `CredentialsInvalid` (`jellyfin-plugin-opensubtitles` `23d00cb`,
    `Jellyfin.Plugin.OpenSubtitles/Configuration/PluginConfiguration.cs:13-23`).
  - TMDb has no `SimilarItemsCacheDays` (`crates/ferrofin-providers/src/builtin_plugins.rs:114-123`). Upstream has it
    at `MediaBrowser.Providers/Plugins/Tmdb/Configuration/PluginConfiguration.cs:131` (gl-220).
- An adopted `LDAP-Auth.xml` is read once and never changed or removed
  (`crates/ferrofin-extensions/src/ldap_auth.rs:48`, `ldap_auth/adoption.rs:1-24`), so it goes stale as soon as an
  admin edits the settings in Ferrofin. A Ferrofin-native install has no such file. Reproduced: saving LDAP-Auth's
  settings through `POST /Plugins/{id}/Configuration` wrote only
  `config/plugins/958aad66-3784-4d2a-b89a-a7b6fab6e25c/config.json`.
- Adoption reads no other file from `plugins/configurations`: the bundled metadata plugins' XML (file names at
  `crates/ferrofin-providers/src/builtin_plugins.rs:109-162`) is not imported. That is an adoption gap (gl-1574). It
  does not change what an export can write.

## The mapping trial (reproduction)

Scratch directories and two docker images:
- `jellyfin/jellyfin:12.4`, the pinned digest `sha256:fea863d0…`;
- Ferrofin built from `1893be47d`.

Ferrofin's first runs used the `v1.5.0` image and gave the same results. The fixture was one M3U channel and an XMLTV
guide with six airings of a series and one film.

1. **Jellyfin as the oracle.** Wizard, tuner, guide, guide refresh, a timer on the film and a series timer on the
   series. Result: `config/livetv.xml`, 7 timers in `data/livetv/timers.json`, 1 in `seriestimers.json`.
2. **Ferrofin on the same fixture.** Ferrofin set up the same timers and stored 7 rows. Against Jellyfin's files:
   - the channel id, all 7 programme ids and the series `SeriesId` were identical;
   - the manual timer's `IsManual` column was 1;
   - the airing timers' external ids follow upstream's `(seriesTimer.Id + ExternalProgramId).GetMD5()`, as
     Jellyfin's do;
   - the stored `Data` has the key set of Jellyfin's own `TimerInfoDto`, less the embedded `ProgramInfo` item the
     API adds when it answers.
3. **The export from Ferrofin's rows.** Jellyfin's server from step 1 was stopped and copied. Both JSON files and
   every `LiveTv*` row in `BaseItems` were removed, because Ferrofin has no such rows to export. Ferrofin's
   `FerrofinLiveTvTimers` and `FerrofinLiveTvSeriesTimers` rows were then written as the two files:
   - upstream's `GetTimerInfo` and `GetSeriesTimerInfo` mapping on `Data`;
   - `Id` = `ExternalId`;
   - `IsManual` and `SeriesId` from their columns.
4. **Jellyfin on the export.** It started and listed the 7 timers and the series timer before any guide refresh.
   The guide refresh recreated the channel and the 7 programmes, kept every timer and scheduled no duplicate airing.
   Every timer and the series timer then had exactly the keys and values Jellyfin wrote in step 1, except the ids,
   which are Ferrofin's own.
5. **Without the two columns.** The same mapping run on the DTOs alone, without Ferrofin's columns, lost two
   things:
   - the manual timer's `IsManual` (`true` → `false`);
   - the series timer's `SeriesId`, so its airings would have been matched by name.
6. **A completed timer.** The Ferrofin fixture recorded nothing (its stream does not play), so no recording row was
   exported. Only an earlier, Jellyfin-sourced copy of the trial tested this path: a synthetic `Completed` entry
   with a `RecordingPath` stayed in place through the guide refresh. What it needs comes from the C# instead:
   - Jellyfin never refreshes a completed timer, so it keeps only what the export writes;
   - the DTO mapping writes none of the programme facts (`EpisodeTitle`, `SeasonNumber`, `EpisodeNumber`,
     `SeriesId`, `ShowId`, `Genres`, `ProviderIds`). The recording row holds the first three (with `ProductionYear`
     and the `Is*` flags), so the export fills those. Jellyfin reads none of the others from a completed timer: the
     API hides it, and only a series fan-out rewrites it;
   - the fields Jellyfin matches a completed timer on (`Id`, `ProgramId`, `ChannelId`, `SeriesTimerId`, `Status`,
     `RecordingPath`, `EndDate`) all come from the mapping and the recording row;
   - `IsManual` decides whether a series fan-out sets it back to `New`, so a completed timer takes its `IsManual`
     column too.

Scope of the proof: an M3U tuner with an XMLTV guide whose times are in UTC. Two cases are unproven:
- HDHomeRun channel ids and Schedules Direct programme ids come from other code on both sides;
- an XMLTV programme id embeds the guide's own offset (`{channel}_{start:O}` over the source `DateTimeOffset`,
  `Jellyfin.LiveTv/Listings/XmlTvListingsProvider.cs:321`), so a guide in another time zone writes `+01:00`.
  Ferrofin ports that (`manager.rs:1006-1022`), but only its unit tests cover it.

It matters: a programme id that
does not match makes Jellyfin delete the timer at its first guide refresh (`DefaultLiveTvService.cs:107-112`).

## D6: what the export can carry

The evidence supports **option A, including timers and recordings**: a safe mapping exists for everything Live TV
keeps, and it is lossless for scheduled timers and series timers, provided Jellyfin's first guide refresh succeeds
(see the risks below).

| Ferrofin source | Jellyfin target | How |
|---|---|---|
| `named/livetv.json` + `FerrofinLiveTvTunerHosts.Data` + `FerrofinLiveTvListingProviders.Data` | `config/livetv.xml` | the served merged document, as XML (the import, reversed); the recording paths rewritten, the tuner URLs unchanged (below) |
| `FerrofinLiveTvTimers` | `data/livetv/timers.json` | upstream's `GetTimerInfo` on `Data`, `Id` = `ExternalId`, plus `IsManual` from its column, completed timers included |
| `FerrofinLiveTvRecordings` for a `Completed` timer | that timer's `RecordingPath` and programme facts | joined on `TimerId` = the timer row's `Id`; the path rewritten into the target layout (D5) |
| `FerrofinLiveTvSeriesTimers` | `data/livetv/seriestimers.json` | upstream's `GetSeriesTimerInfo` on `Data`, `Id` = `ExternalId`, plus `SeriesId` from its column |
| `FerrofinLiveTvChannels`, `FerrofinLiveTvPrograms` | none | Jellyfin rebuilds them from the guide with the same GUIDs; reported as regenerated, not lost |
| recording files and NFO sidecars | the same files | they are library media; D5 decides how they are copied |

What the trial and the C# say about the risks:
- **Jellyfin handles the timer states it reads:**
  - at start (`Jellyfin.LiveTv/Recordings/RecordingsHost.cs:30-31`) an ended timer is deleted
    (`TimerManager.cs:93-108`, `DefaultLiveTvService.cs:540-554`);
  - missing programme facts are copied back at the guide refresh and again when the timer fires (`:571-584`).
- **Until the first guide refresh, Jellyfin has no channel and no programme items.** Two hazards follow, and
  native Jellyfin meets neither:
  - Nothing refreshes the guide at start. The scheduled task first runs an hour after start when it has never
    completed (`Emby.Server.Implementations/ScheduledTasks/Triggers/IntervalTrigger.cs:44-52`); otherwise an admin
    runs it. A timer that fires before then finds no channel (`DefaultLiveTvService.cs:586`, `:598-602`), so
    `RecordStream` throws (`RecordingsManager.cs:307`). The error is logged and the airing is not recorded.
  - A failed first refresh deletes every exported timer, and a partial one every timer whose programmes it did not
    load. The timer refresh runs even when the guide
    refresh had errors (`Jellyfin.LiveTv/Guide/GuideManager.cs:163-164`), and a timer whose programme it cannot
    find is deleted (`DefaultLiveTvService.cs:107-112`). Native Jellyfin keeps its stored programmes across a failed
    refresh, so its timers survive; an export has none stored. A guide path or URL not carried over, an unreachable
    source or a Schedules Direct login that fails would do it.

  Mitigations for gl-1556: the export checks that every tuner and guide source is reachable from the target, and
  the loss report tells the operator to run "Refresh Guide" right after the first start. Writing Ferrofin's
  channels and programmes as `BaseItems` rows would remove both hazards, but it needs the `BaseItems` writer
  audit (gl-1558) first.
- **The ids line up** for an M3U tuner with an XMLTV guide (step 2). A series fan-out after the swap finds the
  exported timers instead of scheduling duplicates (step 4). The gate must repeat this for HDHomeRun and Schedules
  Direct before the claim covers them.
- **A tuner's URL is part of every id.** An M3U channel id is `m3u_` + MD5 of the tuner `Url` + MD5 of the stream
  line (`Jellyfin.LiveTv/TunerHosts/M3UTunerHost.cs:67`, `M3uParser.cs:104`). That channel id is inside every timer's
  `ChannelId` and every XMLTV `ProgramId`, and the channel GUID is derived from it. So the export writes tuner URLs
  unchanged. If D5's layout needs to move a local M3U file, the export recomputes every channel and programme id
  in the timers and the series timers from the new URL (an airing whose `ChannelId` differs from its series
  timer's is cancelled, `DefaultLiveTvService.cs:662-666`), and reports that the channel GUIDs, which favourites hang on, change.
- **The recording paths and `MediaLocationsCreated` move together.** `RecordingPath`, any custom
  `MovieRecordingPath`/`SeriesRecordingPath`, a completed timer's `RecordingPath` and every `MediaLocationsCreated`
  entry are host paths. The export rewrites them into the target layout (D5) together with the recording
  libraries' own paths. One left behind is worse than lost: Jellyfin removes the library of each
  `MediaLocationsCreated` entry that is not a current recording folder at its first start, and the library's
  watch state with it (`RecordingsManager.cs:224-240`). Ferrofin guards the same hazard on adoption
  (`recording_folders.rs:12-21`). A custom folder outside the data directory is reported for the operator to mount.
- **An adopted install's recording paths.** Adoption drops them (above), so unless an admin set them in Ferrofin the
  export writes none, and Jellyfin records to its default `{data}/livetv/recordings`, the same default Ferrofin
  used, as long as D5 carries that folder into the export.
- A recording row whose status is not `Completed` exists only after a crash (the export refuses a running server,
  gl-1550). It maps like any timer, and Jellyfin deletes it as ended at start, keeping the partial file as media,
  which is what Jellyfin does after its own crash.

Option B adds nothing beyond this. Option C would throw away state the trial shows carries over intact.

## D7: plugin settings

The evidence supports **option A**, and it extends past LDAP-Auth:
- **LDAP-Auth:** write `plugins/configurations/LDAP-Auth.xml` from `config.json`.
  - Fill `LdapUsers` from `FerrofinExternalIdentities` for the LDAP-Auth provider id: `LinkedJellyfinUserId` =
    `UserId`, `LdapUid` = `Subject`, `ProfileImageHash` = `AvatarHash`. Without the map, a reinstalled plugin falls
    back to user names and turns a renamed directory user into a new Jellyfin user
    (`LDAPAuthenticationProviderPlugin.cs:108-129`).
  - Always write the file from the current settings, never copy the adopted original, which goes stale.
  - The loss report says LDAP users cannot sign in until the plugin is installed (`UserManager.cs:988-1004`). It
    also lists the certificate and key paths, which are host paths the operator carries over by hand.
- **The bundled metadata plugins** (TMDb, OMDb, MusicBrainz, AudioDb, Studio Images) are part of Jellyfin and read
  their files without any reinstall. An export that leaves them out silently resets an admin's settings (for
  example TMDb's missing-episode options) to defaults. Writing them is the same `config.json` → XML step. A setting
  Ferrofin does not store (TMDb's `SimilarItemsCacheDays`) is left to Jellyfin's default and reported.
- **Intro Skipper, Merge Versions, File Transformation, OpenSubtitles** have Jellyfin counterparts that read the
  files above once they are reinstalled. Write each one whose stored settings match the upstream
  `PluginConfiguration`, map the known differences (OpenSubtitles' `ApiKey` has no upstream field; upstream's
  `CredentialsInvalid` starts `false`), and report the rest by name. The field-by-field check is gl-1556's.
- **Ferrofin Settings** and WASM plugins have no Jellyfin counterpart: report them as lost.

Option B (report as lost) gives up a mapping that is a single serialization step. The operator would have to
re-enter LDAP bind credentials and filters by hand.

## Open points for gl-1556

- How the XML is written: `XmlSerializer`'s shape for each document. gl-1554 builds that writer for named
  configurations; gl-1556 reuses it for `livetv.xml` and the plugin files.
- The gate (gl-1546 and later) should repeat steps 3 and 4 against an export from a real Ferrofin data directory,
  with an HDHomeRun tuner, a Schedules Direct guide and an XMLTV guide in a non-UTC time zone as well as the
  UTC M3U and XMLTV case.
- Favourites and other user data on Live TV channels key on channel items, which an export does not write. Who
  carries them is gl-1559's audit (users and user data).
