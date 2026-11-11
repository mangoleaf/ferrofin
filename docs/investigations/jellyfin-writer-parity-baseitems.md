# Writer parity: `BaseItems` and its child tables

This ledger records, for every column of `BaseItems` and of its item-owned child tables, whether Ferrofin writes the
value Jellyfin would have written for the same item. It is phase P13 (gl-1558) of the "Backwards migration to
Jellyfin" feature.

That feature's **round trip** is: Ferrofin adopts a Jellyfin database, is used, and the result goes back to Jellyfin.
Its **gate** (`gates/roundtrip/`, added by the open pull request for gl-1546) boots the real Jellyfin on what
Ferrofin left. The gate can only claim "no churn"
once every row here matches, is fixed, or is a difference the owner accepted. The feature's decision D8 says: match
upstream by default, and accept a divergence only on the owner's explicit decision.

Each column has a verdict:

- **match**: the values agree.
- **fix**: Ferrofin diverges, and the follow-up issue named makes it match upstream. **fix (verify first)** means the
  divergence is either read in code but not measured, or measured but not yet traced to its writer (the issue says
  which); its triage confirms it before any change.
- **decision**: a divergence proposed to the owner for acceptance. The evidence and an analysis are on the issue, and
  nothing is accepted until the owner answers.
- **owned**: another investigation or audit owns the column. This ledger cites it and gives no verdict of its own.

Values are written **Jellyfin first, then Ferrofin** ("`''` vs NULL": Jellyfin `''`, Ferrofin NULL), everywhere.

- **Code:** Ferrofin at `release/v1.6.0` `1893be47d`; upstream at `v12.4`. Upstream paths name the C# project
  (`Jellyfin.Server.Implementations/…`); Ferrofin paths are repo-relative.
- **Tables:** the pinned schema (`crates/ferrofin-db/tests/data/jellyfin-12.2-schema.sql`) has these item-owned
  tables:
  - this ledger: `BaseItems`, `MediaStreamInfos`, `Chapters`, `AttachmentStreamInfos`, `BaseItemMetadataFields`,
    `BaseItemProviders` and `BaseItemTrailerTypes`;
  - gl-1559: users, user data, devices and keys;
  - gl-1560: people, item values, linked children, ancestors and images, plus `KeyframeData`, `MediaSegments` and
    `TrickplayInfos` (cited below).

## Method

### Static

**Upstream's writer.** Upstream writes `BaseItems` through one mapper:

- `BaseItemMapper.Map` (`Jellyfin.Server.Implementations/Item/BaseItemMapper.cs:257-423`) builds the row.
- `ItemPersistenceService.UpdateOrInsertItems` (`Jellyfin.Server.Implementations/Item/ItemPersistenceService.cs:313`,
  called from `SaveItems`, `:173`) calls it at `:341` and sets `TopParentId` beside it (`:342`).
- Every save rewrites every column.

**Ferrofin's full-row writers.**

- The scan's upsert: `UPSERT_SQL` (`crates/ferrofin-core/src/item_persistence_service.rs:5649`), bound by
  `columns_of` (`:1943`). It is reached through `save_items` and `save_scanned_items`, by the scan, the library
  folders and Live TV.
- The inserts that bypass it, all in `crates/ferrofin-core/src/`:
  - the scan's by-name batch insert (`item_persistence_service.rs:177`) and `ensure_by_name_item` (`:2943`);
  - `insert_named_item_in` (`:423`), `insert_root_folder` (`:719`) and `rekey_container` (`:759`);
  - the person inserts (`people_repository.rs:75,442`);
  - `library_structure_repository.rs:155` and `user_view_repository.rs:406`.

**Ferrofin's partial writers.** There are about 80 production `UPDATE "BaseItems"` statements (outside test modules). They set a few columns
each, mostly:

- ids and keys (`ParentId`, `TopParentId`, `OwnerId`, `PrimaryVersionId`, `PresentationUniqueKey`);
- dates (`DateCreated`, `DateModified`, `DateLastSaved`, `DateLastRefreshed`, `DateLastMediaAdded`);
- `Path`, `Data`, `CleanName` and `SortName`;
- and in a few places metadata: the extras copy (`item_persistence_service.rs:7417`), person enrichment
  (`people_repository.rs:2006`), inherited ratings (`item_persistence_service.rs:1530,3880`) and `LUFS`
  (`scheduled_tasks/library.rs:365`).

They were not compared statement by statement. Each column's measured end state (E1 to E4) includes their effect.
The ones that need data the fixtures do not hold (person enrichment from providers, most adoption repairs in
`adoption_repairs.rs`) are listed under "Not covered".

### Empirical

Four comparisons on the adoption gate's synthetic fixtures (2026-11-10, the image of `1893be47d`,
`jellyfin/jellyfin:12.4`). Each pair of databases was diffed row by row: items matched by `Id`, then by
`(Type, Path)`, then by `(Type, Name)`; child rows by the matched item and their own key.

- **E1, fresh writes (12.4).** A fresh Ferrofin was seeded exactly as the `jellyfin-12.4-fresh` fixture was (the
  `seed()` function in `gates/adoption/synthetic_fixture.py`, on the same media). Its database was diffed with the
  fixture's: the same library, the same API calls, each server's own writers.
  - Jellyfin has 68 items and Ferrofin 67. 66 matched: 62 by the same `Id`, and four by type and name only, because
    their ids differ (Genre ×3 and MusicGenre ×1, gl-1597).
  - Jellyfin has two `Playlists` `UserView` rows that Ferrofin lacks (gl-1584), and Ferrofin has a `Year` row that
    Jellyfin lacks (gl-1583).
  - The seed's collection step waits for the scan Jellyfin queues when the first collection creates the Collections
    library. Ferrofin queues none (gl-1598), so that run treated the step as done once the server was idle.
- **E2, the return (12.4 from 12.3).** Ferrofin adopted `jellyfin-12.4-from-123`, scanned it and ran the gate's
  usage stage (`gates/roundtrip/usage.py`, added by the gate's pull request for gl-1546, not yet merged). Jellyfin 12.4
  then booted on a copy and ran its first library scan, and the database before and after that scan was diffed.
  - On this route Jellyfin rewrote only the rows it refreshed: the new playlist and box set, to its defaults, and the
    music albums' `PremiereDate` text.
  - Every other difference outlived the return.
- **E3, adoption (12.4 from 12.3).** The fixture as Jellyfin wrote it, diffed with what Ferrofin left after the first
  half of E2. Ferrofin's adoption and scan rewrite these in a Jellyfin database:
  - a Book's `SeriesId` (gl-1580);
  - the CJK `SortName` (gl-1582);
  - the music artists' `RunTimeTicks` (gl-1594);
  - the albums' `PremiereDate` text (gl-1565).

  They also add a `Year` row (gl-1583), and the usage stage adds a playlist, a box set and two `UserView` rows. The
  `Live TV` view among them follows upstream's guide-refresh path (see "Rows only one side writes").
- **E4, adoption (10.11.8).** Ferrofin adopted, scanned and used the `jellyfin-10.11.8` fixture. The result was
  diffed with Jellyfin 12.4's own upgrade of the same database (the `jellyfin-12.4-from-10` fixture).
  - Jellyfin cannot boot on this route until the export rewrites the EF history (gl-1549), so there is no return
    run here.
  - Everything E1 to E3 found shows here too. This route adds the alternate-version movie's name and bitrate, its
    streams' values and the `Playlists` view's id (gl-1599).

Timestamps that differ only because the runs happened at different times are matches; their text form is gl-1565's.
The gate's row-level diff (gl-1568) will automate these comparisons.

## Follow-ups

| Issue | Verdict | What |
|---|---|---|
| gl-1580 | fix | NULL where Jellyfin writes `''`, `0`, `Unknown`, `Other` or the zero GUID, on every item type; the scan rewriting adopted defaults to NULL; `rekey_container` copying 17 of 72 columns. The cause behind gl-1562 |
| gl-1581 | fix | `Path` stored resolved where upstream stores `%AppDataPath%`/`%MetadataPath%` (with gl-1209 and gl-1220) |
| gl-1582 | fix | `SortName` without upstream's ICU transliteration, and digit padding for ASCII digits only |
| gl-1583 | fix | `Year` rows created by every scan |
| gl-1584 | fix | the user's `Playlists` view row not created by user-scoped queries |
| gl-1585 | fix | the PLACEHOLDER row's `CleanName` |
| gl-1586 | fix | dates on rows created outside the scan (root, user root, by-name items), and new playlists' and box sets' `DateModified` |
| gl-1587 | fix | NFO `<country>` never stored in `ProductionLocations` |
| gl-1588 | fix | the metadata editor's list fields and `OriginalTitle` |
| gl-1589 | fix | `LUFS` parsed as `f64` instead of a widened `float` |
| gl-1590 | fix (verify first) | an imageless chapter's `ImageDateModified` |
| gl-1591 | fix | Live TV channel and programme rows (with gl-763 and gl-1503) |
| gl-1592 | fix (verify first) | a provider key whose case changed leaves the old row |
| gl-1593 | fix (verify first) | a path-less season's `IsVirtualItem` |
| gl-1597 | fix | by-name Genre, Studio and MusicGenre ids not derived as upstream derives them |
| gl-1598 | fix | the first collection's Collections library gets no library scan |
| gl-1599 | fix (verify first) | the 10.11.8 route, measured and not yet traced: the alternate-version movie's name and bitrate, its streams' values, the `Playlists` view's id |
| gl-1594 | decision | music artists' `RunTimeTicks`: Ferrofin sums; upstream's refresh order, read from the code, leaves 0 |
| gl-1595 | decision | extras copy their owner's metadata after the owner's refresh (Ferrofin) or before it (upstream) |

Each of them blocks gl-1573, and so do the open issues these verdicts rely on (gl-763, gl-1503, gl-1209 and
gl-1220) and gl-1596, the owner decision on trickplay `Bandwidth`, filed from these measurements for gl-1560's table.

The rows owned elsewhere are cited as:

- gl-1562: new Playlist and BoxSet rows (`docs/investigations/jellyfin-parity-new-row-defaults.md`);
- gl-1563: `Data` (`docs/investigations/jellyfin-parity-data-blob.md`);
- gl-1565: date text (`docs/investigations/jellyfin-parity-datetime-text.md`);
- gl-1566: the playlists folder's dates (`docs/investigations/jellyfin-parity-playlists-view.md`);
- gl-1560: the three tables above.

The investigation file names are the ones proposed on those issues.

## `BaseItems`

"Map" is `BaseItemMapper.cs` at the line given. "Extras" means the owner's metadata copied onto a movie's extras
(gl-1595): in E1 the one extra got the owner's `CommunityRating`, `OfficialRating`, `Overview` and `Genres`, and
the inherited ratings scored from them.

| Column | Upstream | Ferrofin | Evidence | Verdict |
|---|---|---|---|---|
| `Id` | Map:263; by-name items derive it from type and path (`LibraryManager.cs:1390-1395,797-820`) | path-derived for media and people; the shared `ItemValues` id for Genre and Studio (`item_persistence_service.rs:210,283-289`); a differently derived MusicGenre id | E1: 62 equal; Genre ×3 and MusicGenre ×1 differ | fix: gl-1597 (by-name rows) |
| `Album` | Map:297 | upsert | E1 equal (audio) | match |
| `AlbumArtists` | Map:382, `IHasAlbumArtist`, `\|`-joined distinct | upsert | E1 equal | match |
| `Artists` | Map:377, `IHasArtist` | upsert | E1 equal | match |
| `Audio` | Map:324, Live TV `ProgramAudio` | never written for programmes | static | fix: gl-763 |
| `ChannelId` | Map:309, a `Guid` (zero when unset) | NULL off Live TV; the channel's id on programmes | E1: 65 rows zero GUID vs NULL; E4: the `Playlists` view; programmes static | fix: gl-1580 (Playlist and BoxSet rows: gl-1562) |
| `CleanName` | Map:279, `GetCleanValue` (`src/Jellyfin.Extensions/StringExtensions.cs:159-176`) | `get_clean_value` (`crates/ferrofin-util/src/string_extensions.rs:266-273`), the same algorithm | E1, E3 equal except the PLACEHOLDER row; E4 the alternate-version movie | fix: gl-1585 (placeholder); fix (verify first): gl-1599 (E4) |
| `CommunityRating` | Map:274; Live TV programmes from the guide (`src/Jellyfin.LiveTv/Guide/GuideManager.cs:535`) | upsert, NFO, TMDB; never for programmes | E1 equal except the extra (NULL vs 7.5); programmes static | match; extras: gl-1595; Live TV: gl-763 |
| `CriticRating` | Map:293, `float` widened | editor, NFO, OMDb (f32 widened) | static; copied to extras upstream and here | match; extras: gl-1595 |
| `CustomRating` | Map:275 | editor, NFO | static; copied to extras | match; extras: gl-1595 |
| `Data` | Map:268 | upsert | E1: 47 rows differ, across most kinds | owned: gl-1563 (told of the other kinds on 2026-11-10) |
| `DateCreated` | Map:307 (MinValue → NULL) | upsert; the scan's by-name batch insert and the person inserts omit it (`ensure_by_name_item` binds it) | E1: set vs NULL on Person ×5, Genre ×3, MusicGenre ×1; others differ by run time | fix: gl-1586; text: gl-1565 |
| `DateLastMediaAdded` | Map:418, folders | upsert | E1: playlists differ by run time and text | match; text: gl-1565 |
| `DateLastRefreshed` | Map:310 | upsert; root, user root and by-name rows never refreshed | E1: set vs NULL on AggregateFolder, UserRootFolder, Genre ×3, MusicGenre ×1, PlaylistsFolder | fix: gl-1586; playlists folder: gl-1566 |
| `DateLastSaved` | Map:311, stamped on every repository save | upsert stamps updates; root, user root, by-name batch and person inserts omit it | E1: set vs NULL on AggregateFolder, UserRootFolder, Person ×5, Genre ×3, MusicGenre ×1 | fix: gl-1586 |
| `DateModified` | Map:308 (MinValue → NULL) | upsert; `insert_named_item_in` sets it on playlists and box sets; the by-name batch and person inserts omit it | E1: NULL vs set on Playlist ×4, BoxSet ×2 and the PlaylistsFolder; set vs NULL on Person ×5; Genre and MusicGenre rows NULL on both; media rows text only | fix: gl-1586; playlists folder: gl-1566; text: gl-1565 |
| `EndDate` | Map:273 | Live TV programmes (`crates/ferrofin-livetv/src/projection.rs:686`); person enrichment | static | match |
| `EpisodeTitle` | Map:348, `IHasProgramAttributes` | `program_entity` | static | match |
| `ExternalId` | Map:304 | `program_entity` | static | match |
| `ExternalSeriesId` | Map:301; set only when non-blank and different (`src/Jellyfin.LiveTv/Guide/GuideManager.cs:540-543`) | `program_item_update` overwrites it | static | fix: gl-1591 |
| `ExternalServiceId` | Map:354, LiveTvChannel | `channel_entity` (`Emby`) | static | match |
| `ExtraType` | Map:329 | upsert | E1 equal | match |
| `ForcedSortName` | Map:286 | upsert, editor | E1 equal (the edited movie) | match |
| `Genres` | Map:306, always joined (`''` when empty) | NULL when empty; Live TV joins without dedupe | E1: 44 rows `''` vs NULL, among them the extra (`''` vs `Adventure`, its owner's genre); other non-empty values equal | fix: gl-1580; extras: gl-1595; Live TV: gl-1591; editor: gl-1588 |
| `Height` | Map:314, `int` (0); 720 for an HD guide programme (`GuideManager.cs:596-600`) | NULL unless probed or from EXIF; programmes none | E1: 47 rows 0 vs NULL; probed values equal | fix: gl-1580 (Playlist and BoxSet rows: gl-1562); Live TV: gl-763 |
| `IndexNumber` | Map:276 | upsert | E1 equal | match |
| `InheritedParentalRatingSubValue` | Map:292, from the item's own rating | upsert, rating repairs | E1 equal except the extra (NULL vs 0) | match; extras: gl-1595 |
| `InheritedParentalRatingValue` | Map:291 | as above | E1 equal except the extra (NULL vs 0) | match; extras: gl-1595 |
| `IsFolder` | Map:419 | upsert | E1 equal | match |
| `IsInMixedFolder` | Map:290 | upsert | E1 equal | match |
| `IsLocked` | Map:277 | upsert, editor | E1 equal (the locked movie) | match |
| `IsMovie` | Map:346; channels store their programmes' aggregate (`GuideManager.cs:275-313`) | programmes yes; channels derive it at read time | static | match for programmes; fix: gl-1591 (channels) |
| `IsRepeat` | Map:349 | `program_entity` | static | match |
| `IsSeries` | Map:347; channel aggregate as for `IsMovie` | as for `IsMovie` | static | match for programmes; fix: gl-1591 (channels) |
| `IsVirtualItem` | Map:300; seasons re-derived (`MediaBrowser.Providers/TV/SeasonMetadataService.cs:105-114`) | programmes `true`; seasons never re-derived | static | fix (verify first): gl-1593 |
| `LUFS` | Map:298; `float.Parse` widened (`Emby.Server.Implementations/ScheduledTasks/Tasks/AudioNormalizationTask.cs:279`) | `parse::<f64>()` (`crates/ferrofin-core/src/scheduled_tasks/library.rs:290`) | static (the fixtures' LUFS scan is off) | fix: gl-1589 |
| `MediaType` | Map:405, `Unknown` unless overridden | NULL from the non-scan inserts | E1: 21 rows `Unknown` vs NULL; E4: the `Playlists` view | fix: gl-1580 (Playlist and BoxSet rows: gl-1562) |
| `Name` | Map:278 | upsert | E1, E3 equal; E4: the alternate-version movie (`Twin Horizon` vs `Twin Horizon - 720p`) | match in E1 to E3; fix (verify first): gl-1599 (E4) |
| `NormalizationGain` | Map:299, from tags | `crates/ferrofin-core/src/audio_tags.rs:192` (f32 widened) | static | match |
| `OfficialRating` | Map:280 | upsert | E1 equal except the extra (NULL vs `G`) | match; extras: gl-1595 |
| `OriginalLanguage` | Map:296 | editor, TMDB | static | match |
| `OriginalTitle` | Map:295; the editor keeps spaces | the editor trims (`crates/ferrofin-api/src/handlers/item_update.rs:394`) | static | fix: gl-1588 |
| `Overview` | Map:281; photos copy the EXIF comment (`Emby.Photos/PhotoProvider.cs:100`) | upsert; photos NULL | E1: Photo ×2 `''` vs NULL; the extra NULL vs its owner's overview | fix: gl-1580 (photos); extras: gl-1595 |
| `OwnerId` | Map:312 (empty → NULL) | upsert, version and extra repairs | E1 equal through the item match | match |
| `ParentId` | Map:271 | upsert, structure repairs | E1 equal through the item match | match |
| `ParentIndexNumber` | Map:282 | upsert | E1 equal | match |
| `Path` | Map:272, `ReverseVirtualPath` | resolved, except box sets, playlists and the Collections folder | E1: 10 rows (Genre ×3, MusicGenre ×1, Person ×5, PlaylistsFolder); E4: the `Playlists` view | fix: gl-1581 |
| `PreferredMetadataCountryCode` | Map:289 | editor | static | match |
| `PreferredMetadataLanguage` | Map:288 | editor | static | match |
| `PremiereDate` | Map:283 | upsert, person enrichment | E1: 14 rows differ in text only | owned: gl-1565; Live TV original air date: gl-763 |
| `PresentationUniqueKey` | Map:294 | upsert, key repairs | E1, E3 equal; E4: the `Playlists` view | match in E1 to E3; fix (verify first): gl-1599 (E4) |
| `PrimaryVersionId` | Map:359, `Video` | upsert, version repairs | E1, E4 equal | match |
| `ProductionLocations` | Map:332, joined, blanks dropped | NULL when empty; NFO countries dropped | E1: 64 rows `''` vs NULL | fix: gl-1580 (Playlist and BoxSet rows: gl-1562); NFO: gl-1587; editor: gl-1588; extras: gl-1595 |
| `ProductionYear` | Map:284 | upsert | E1, E4 equal | match |
| `RunTimeTicks` | Map:287; cumulative for MusicArtist (`MediaBrowser.Providers/Manager/MetadataService.cs:485-503`) | upsert; cumulative after every scan (`crates/ferrofin-core/src/library_scan.rs:10206`) | E1, E4: MusicArtist ×2 0 vs the sum; E3 rewrites | decision: gl-1594 |
| `SeasonId` | Map:372, Episode (zero GUID when unset) | upsert; NULL for an episode outside a season | E1 equal for episodes in seasons; loose episodes static | fix: gl-1580 (loose episodes) |
| `SeasonName` | Map:371 | upsert | E1 equal | match |
| `SeriesId` | Map:365, `IHasSeries` (zero GUID when unset) | NULL for Books; the scan rewrites an adopted zero GUID to NULL | E1, E3, E4: the Book row | fix: gl-1580 |
| `SeriesName` | Map:364 | upsert | E1 equal | match |
| `SeriesPresentationUniqueKey` | Map:366 | upsert, key repairs | E1, E4 equal | match |
| `ShowId` | Map:387, LiveTvProgram | `program_entity` | static | match |
| `Size` | Map:305 | upsert | E1 equal | match |
| `SortName` | Map:285; `ModifySortChunks`: Unicode digit padding, then ICU transliteration (`MediaBrowser.Controller/Entities/BaseItem.cs:1081-1099`); per-kind overrides (Audio, Episode, Season, LiveTvChannel) | `modify_sort_chunks` (`crates/ferrofin-util/src/sort_name.rs:217-236`): ASCII digits, no transliteration; the per-kind sort names come from the scan's derivations | E1, E3, E4: `cafe unicode dong jing` vs `cafe unicode 東京`; episodes, seasons and audio equal | fix: gl-1582 |
| `StartDate` | Map:408, `IHasStartDate` | `program_entity` | static | match |
| `Studios` | Map:333, joined | NULL when empty | E1: 64 rows `''` vs NULL | fix: gl-1580 (Playlist and BoxSet rows: gl-1562); editor: gl-1588; extras: gl-1595 |
| `Tagline` | Map:302 | editor, NFO, TMDB | static | match |
| `Tags` | Map:334, joined; channels get `Kids` | NULL when empty; no `Kids` on channels | E1: 64 rows `''` vs NULL | fix: gl-1580 (Playlist and BoxSet rows: gl-1562); channels: gl-1503 |
| `TopParentId` | `ItemPersistenceService.cs:342` | upsert, structure repairs | E1 equal through the item match | match |
| `TotalBitrate` | Map:303 | upsert | E1, E3 equal; E4: the alternate-version movie (825 vs 192825, as measured, while its stream `BitRate` is NULL vs 825) | match in E1 to E3; fix (verify first): gl-1599 (E4) |
| `Type` | Map:262 | upsert | E1 equal on every matched row | match; rows only one side writes: gl-1583, gl-1584 |
| `UnratedType` | Map:411, `GetBlockUnratedType()` | the scan (`crates/ferrofin-core/src/library_scan.rs:5033`), Live TV (`crates/ferrofin-livetv/src/projection.rs:339,736`) and `update_rating` (`item_persistence_service.rs:3880`); the other inserts NULL | E1: 19 rows `Other` vs NULL; E4: the `Playlists` view | fix: gl-1580 |
| `Width` | Map:313, `int` (0); 1280 for an HD guide programme | as for `Height` | E1: 47 rows 0 vs NULL | fix: gl-1580 (Playlist and BoxSet rows: gl-1562); Live TV: gl-763 |

### Rows only one side writes

- **`Year` rows.** Upstream creates one only for `/Years` (`Jellyfin.Api/Controllers/YearsController.cs:176-179,222-226`).
  Ferrofin also creates one for every distinct year after each scan. Fix: gl-1583.
- **`UserView` rows.** Upstream creates the user's `Playlists` view row on any user-scoped query without a parent
  (`Emby.Server.Implementations/Library/LibraryManager.cs:2090-2117`). Ferrofin creates it only for `/UserViews`.
  Fix: gl-1584; its id on the 10.11.8 route is gl-1599's.
- **The `Live TV` view.** Ferrofin created it in E3, and that matches upstream: the guide refresh and the Live TV
  routes create it without a tuner on both servers (`src/Jellyfin.LiveTv/Guide/GuideManager.cs:192`, through
  `GetInternalLiveTvFolder`, `src/Jellyfin.LiveTv/LiveTvManager.cs:1271-1275`). The row matches.
  - The timing differs slightly. With no task history, upstream runs that refresh an hour after booting
    (`Emby.Server.Implementations/ScheduledTasks/Triggers/IntervalTrigger.cs:44-47`).
  - Ferrofin adds a per-task spread of up to 55 minutes to that hour (`crates/ferrofin-core/src/scheduled_tasks.rs:69-98`).
    That is a scheduling difference outside this ledger.
- **Orphans and alternate versions on the 10.11.8 route.** The two orphaned episodes and the 720p version row are
  removed or re-keyed by both Jellyfin 12.4's upgrade and Ferrofin's adoption (E4). Match, apart from the movie's
  name and bitrate (gl-1599).

## Child tables

### `MediaStreamInfos`

Upstream deletes and re-adds the rows in a transaction (`Jellyfin.Server.Implementations/Item/MediaStreamRepository.cs:44-45`),
with `Map` at `:194-249`. Ferrofin does the same in `crates/ferrofin-core/src/media_stream_repository.rs:207-285`, built by
`stream_dto_to_entity` (`crates/ferrofin-core/src/media_source_manager.rs:1484`).

- **E1:** all 54 rows matched in every column.
- **E4:** on the alternate-version movie's streams, `BitRate` is NULL vs 825 and `Height`, `Width`, `IsAvc` and
  `Level` are NULL vs 0 (gl-1599).
- **Not exercised by the fixtures, but mapped one to one in code:** the Dolby Vision fields, `Hdr10PlusPresentFlag`,
  `IsOriginal` and `Comment`. `KeyFrames` is NULL on both.

| Column | Verdict |
|---|---|
| `ItemId`, `StreamIndex`, `AspectRatio`, `AverageFrameRate`, `BitDepth`, `BlPresentFlag`, `ChannelLayout`, `Channels`, `Codec`, `CodecTag`, `CodecTimeBase`, `ColorPrimaries`, `ColorSpace`, `ColorTransfer`, `Comment`, `DvBlSignalCompatibilityId`, `DvLevel`, `DvProfile`, `DvVersionMajor`, `DvVersionMinor`, `ElPresentFlag`, `IsAnamorphic`, `IsDefault`, `IsExternal`, `IsForced`, `IsHearingImpaired`, `IsInterlaced`, `KeyFrames`, `Language`, `NalLengthSize`, `PixelFormat`, `Profile`, `RealFrameRate`, `RefFrames`, `Rotation`, `RpuPresentFlag`, `SampleRate`, `StreamType`, `TimeBase`, `Title`, `Hdr10PlusPresentFlag`, `IsOriginal` | match |
| `BitRate`, `Height`, `Width`, `IsAvc`, `Level` | match in E1; fix (verify first): gl-1599 (E4, the alternate-version movie's streams: `BitRate` NULL vs 825, the others NULL vs 0) |
| `Path` | fix: gl-1581. Upstream reverses it (`:209`) and Ferrofin writes it raw. Only stream files under the data or metadata directory are affected; the read side is gl-1063 |

### `Chapters`

Upstream deletes and re-adds (`Jellyfin.Server.Implementations/Item/ChapterRepository.cs:74-107`); Ferrofin does the same in
`crates/ferrofin-core/src/chapter_repository.rs:63-101`. E1 matched all 80 rows; `ImageDateModified` differed only by
run time.

| Column | Verdict |
|---|---|
| `ItemId`, `ChapterIndex`, `StartPositionTicks`, `Name` | match |
| `ImagePath` | match: both store the resolved path |
| `ImageDateModified` | match when an image exists. For an imageless chapter upstream keeps `DateTime.MinValue` and Ferrofin stores chrono's epoch: fix (verify first) gl-1590. Every fixture chapter has an image |

### `AttachmentStreamInfos`

Upstream clears, then adds (`Jellyfin.Server.Implementations/Item/MediaAttachmentRepository.cs:30-37`); Ferrofin does the same in
`crates/ferrofin-core/src/media_attachment_repository.rs:92-122`. E1 matched the one row.

| Column | Verdict |
|---|---|
| `ItemId`, `Index`, `Codec`, `CodecTag`, `Comment`, `Filename`, `MimeType` | match |

### `BaseItemMetadataFields`

Upstream writes `LockedFields` from Map:335 and replaces the set wholesale with the item's other child rows
(`Jellyfin.Server.Implementations/Item/ItemPersistenceService.cs:485`). Ferrofin adds them on scan (`add_locked_fields`,
`crates/ferrofin-core/src/item_persistence_service.rs:4121`) and replaces them from the editor
(`replace_locked_fields`, `:4091`). The resulting set is the same. E1 matched the locked movie's three rows.

| Column | Verdict |
|---|---|
| `Id`, `ItemId` | match |

### `BaseItemProviders`

Upstream replaces an item's provider rows on every save (Map:315, `Jellyfin.Server.Implementations/Item/ItemPersistenceService.cs:483`). Ferrofin's scan
upserts them additively (`crates/ferrofin-core/src/library_scan.rs:9153`,
`crates/ferrofin-core/src/item_persistence_service.rs:2713`), and rewrites the set only when a recorded key vanished
(`library_scan.rs:23227`). E1 and E4 matched all 10 rows.

| Column | Verdict |
|---|---|
| `ItemId`, `ProviderId`, `ProviderValue` | match. Two gaps: a key whose case changed leaves the old row, fix (verify first) gl-1592; and Live TV writes no provider rows, fix gl-1591 |

### `BaseItemTrailerTypes`

Upstream writes trailer types only for `Trailer` items (Map:397). In practice only channel items carry them
(`src/Jellyfin.LiveTv/Channels/ChannelManager.cs:1072`). Ferrofin produces no item that carries trailer
types, so it has no writer, and no fixture has a row on either side.

| Column | Verdict |
|---|---|
| `Id`, `ItemId` | match, with no rows on either side: nothing Ferrofin creates has trailer types |

### `KeyframeData`, `MediaSegments`, `TrickplayInfos`

Owned by gl-1560, which got this ledger's measurements on 2026-11-10:

- `KeyframeData` 15/15 rows equal.
- `TrickplayInfos` 16/16 equal apart from `Bandwidth`, 289 vs 285 on every row. The issue for that column, gl-1596,
  is already open for the owner. The formula is the same, so the cause is most likely the tile encoder, but the tile
  bytes were not compared.
- `MediaSegments` kept unchanged by the adoption in E3 and E4.

## Not covered

- **Live TV rows.** No fixture configures a tuner, so all Live TV rows were compared from the code alone.
- **Columns the fixtures never populate.** These were compared from the code alone:
  - `Audio`, `ChannelId` on programmes, `CriticRating`, `CustomRating`, `EndDate`, `EpisodeTitle`, the
    `External*` columns;
  - `IsMovie`, `IsRepeat`, `IsSeries`, `IsVirtualItem`, `LUFS`, `NormalizationGain`;
  - `OriginalLanguage`, `OriginalTitle`, `PreferredMetadata*`, `ShowId`, `StartDate`, `Tagline`;
  - the non-empty values of `ProductionLocations`, `Studios` and `Tags`.
- **Partial writers the fixtures cannot reach.** These were not compared:
  - the person enrichment from metadata providers (`people_repository.rs:2006`: `Overview`, `PremiereDate`,
    `EndDate`, `ProductionLocations`), since the fixtures run with providers blocked;
  - the adoption repairs for shapes the clean fixtures lack (`adoption_repairs.rs`). The adoption gate's messy and
    scale fixtures check those against Jellyfin's own results.
- **The return on the 10.11.8 route, the other source routes, and the messy and scale fixtures.**
  - The 10.11.8 return waits on gl-1549, since Jellyfin cannot boot on that route yet.
  - The other routes and fixtures were not run here. The gate's route matrix (gl-1567) and row-level diff (gl-1568)
    cover them once they exist.
