# Writer parity: people, item values, links, ancestors and images

Ledger for phase P15 (gl-1560) of the "Backwards migration to Jellyfin" milestone: every column Ferrofin writes in the credit, item-value, linked-children, ancestor, image and extracted-media tables, compared with what Jellyfin 12.4 writes, with a verdict each. Under decision D8 the default verdict is to fix toward upstream. An accepted divergence needs the owner's explicit decision, so this ledger only proposes them. Code is cited at `release/v1.6.0` `978bea505`. Upstream is Jellyfin `v12.4` (`UPSTREAM_TAG`), cited relative to its checkout. The pinned schema is `crates/ferrofin-db/tests/data/jellyfin-12.2-schema.sql`.

**Tables (45 columns, counted in the pinned schema):** `Peoples` (3), `PeopleBaseItemMap` (5), `ItemValues` (4), `ItemValuesMap` (2), `LinkedChildren` (4), `AncestorIds` (2), `BaseItemImageInfos` (8), `KeyframeData` (3), `MediaSegments` (6), `TrickplayInfos` (8).

Out of this ledger: the `BaseItems` rows of these items, `Person` items included, are gl-1558's ledger ([`jellyfin-writer-parity-baseitems.md`](jellyfin-writer-parity-baseitems.md), PR 906); the account tables are gl-1559's ([`jellyfin-writer-parity-users.md`](jellyfin-writer-parity-users.md), PR 907).

**Verdicts:**
- **match:** same value and text form.
- **fix:** Ferrofin differs. The row names the issue that tracks it, filed by this audit or already open. **fix (verify first)** means the divergence was read in code on both sides and confirmed by an independent code check, but not measured: no fixture reaches it. Its triage reproduces it before any change.
- **recorded:** a divergence an owner decision accepts, or one the code or the maintainers' out-of-scope record lists as a Jellyfin bug not ported; it is cited.
- **owner?:** a possible accepted divergence the owner has not decided. Each is proposed below (P1 to P8), or waits on its own issue. **owner? (documented)** is a deliberate Ferrofin difference a code comment explains, with no owner decision yet.
- **owned:** an investigation or another issue owns the row. This ledger cites it and links its file, or the issue where no file exists yet, and gives no verdict of its own. Per gl-1560's scope, `AncestorIds` for new playlists is gl-1561's ([`jellyfin-parity-playlist-ancestors.md`](jellyfin-parity-playlist-ancestors.md), PR 931; its fix is gl-1626), and the album rows and the playlist collages' `Id` and `Blurhash` are gl-1564's ([`jellyfin-parity-album-covers.md`](jellyfin-parity-album-covers.md), PR 937; its fix is gl-1633). The tracks' Primary rows are gl-692's, outside gl-1564's scope.
- **Edges.** A verdict can add `; <edge>` for one case where the column differs from that verdict: **edge fix** (the named issue tracks it), **recorded edge** (the code or the maintainers' out-of-scope record records it), **owner? edge** (a proposal below covers it) or **owned edge** (an investigation or issue owns it). For example, **match; edge fix** matches except in an edge case the named issue tracks, and **recorded; edge fix** is a recorded divergence with a further edge to fix. `(documented)` after `owner?` means a code comment explains it, as above.
- **match; not compared for …:** the column matches where it was compared. The cases named were not compared, and "Not covered" lists them.

**Abbreviations.** Lower case names a Ferrofin file, capitalised an upstream one:
- `ips`: `crates/ferrofin-core/src/item_persistence_service.rs`; `IPS`: `Jellyfin.Server.Implementations/Item/ItemPersistenceService.cs`;
- `ppl`: `crates/ferrofin-core/src/people_repository.rs`; `PPL`: `Jellyfin.Server.Implementations/Item/PeopleRepository.cs`;
- `di`: `crates/ferrofin-core/src/dynamic_images.rs`;
- `Map`: `Jellyfin.Server.Implementations/Item/BaseItemMapper.cs`; `LM`: `Emby.Server.Implementations/Library/LibraryManager.cs`;
- `msm`: `crates/ferrofin-core/src/media_segment_manager.rs`; `MSM`: `Jellyfin.Server.Implementations/MediaSegments/MediaSegmentManager.cs`;
- `tm`: `crates/ferrofin-core/src/trickplay_manager.rs`; `TM`: `Jellyfin.Server.Implementations/Trickplay/TrickplayManager.cs`.

## Method

**Static.** For each table, every production `INSERT`, `UPDATE` and `DELETE` (test modules excluded) was found by searching the table name, with the callers of the shared writers (`update_people`, `save_item_values`, `item_values_of`, `set_ancestors`, `upsert_linked_child`, `write_item_images`). Each was compared with upstream's writer at `v12.4`. The upstream writers are:
- **credits:** `PeopleRepository.UpdatePeople` through `LibraryManager.UpdatePeopleAsync`, `ItemPersistenceService.DeleteItem`, `PeopleValidationTask`, and the migration `MergeDuplicatePeople`;
- **values, links and ancestors:** `ItemPersistenceService.UpdateOrInsertItems`, which on every save writes the item's values, ancestors and links (IPS:313-790, `GetItemValuesToSave` at :792-815); `DeleteItem` (IPS:51-158) and `UpdateInheritedValues` (IPS:161-170); `LinkedChildrenService` (`Jellyfin.Server.Implementations/Item/LinkedChildrenService.cs`), `CollectionManager`, `VideosController.MergeVersions` and `DeleteAlternateSources`, and `LibraryManager.DeleteItem`;
- **migration routines:** every routine under `Jellyfin.Server/Migrations/Routines/` at `v12.4` that writes one of the ten tables, listed under "Upstream migration routines" with what Ferrofin does for the same databases;
- **images:** `BaseItemMapper.MapImageToEntity`, `ItemPersistenceService` (`SaveImagesAsync`, `UpdateOrInsertItems`, `DeleteItem`), `LibraryManager.UpdateImagesAsync`, `ImageSaver`, `BaseDynamicImageProvider` and its subclasses, and `CollectionFolderImageProvider`;
- **extracted media:** `KeyframeRepository` behind the HLS `CacheDecorator`, `MediaSegmentManager` and `MediaSegmentExtractionTask`, `TrickplayManager`, and `ExternalDataManager` for the deletes.

**Empirical.** Three scratch runs (2026-11-10, local time). Timestamps quoted from the databases are UTC, as both servers store them, so they read 2026-11-11. Both servers ran in the official `jellyfin/jellyfin:12.4` image, Ferrofin's binary mounted into it, on the same synthetic media (`gates/adoption/synthetic_media.generate`), seeded through the API by `gates/adoption/synthetic_fixture.seed`. Both servers' outbound HTTP went to a dead proxy, so no remote provider ran.
- **J:** a fresh Jellyfin 12.4.
- **F:** a fresh Ferrofin, a build of `978bea505`. F used a copy of the seed patched in one step: the first collection waits for the server to go idle instead of for the library scan Jellyfin queues when that collection creates the Collections library. Ferrofin queues none, which is the owner question gl-1598.
- **A:** the same Ferrofin build adopting a copy of J's database and rescanning it.

There is no return run: Jellyfin was not booted on F or A for these tables.

The seed steps that reach these tables:
- the library scans: music with two artists, TV, movies with a local 720p version of `Twin Horizon` and an extra, photos, a book;
- a metadata edit of one movie with a manual genre, which re-saved its credits ("Manually edited orchard"), then a Primary upload on it;
- four playlists by API, one of them path-less;
- two box sets by API.

No fixture has tags, studios, a non-ASCII or case-variant person name, a merge or split of versions, a removal from a box set, a Live TV tuner, a remote image or a media segment.

**Comparison.** A scratch row diff, a script that is not in the repository, matched items by `Id`, then `(Type, Path)`, then `(Type, Name)`; values by `(Type, Value)`; images by item and `ImageType`. It lists at most six one-sided rows, and it keys `Peoples` by `Name` and credits by `(item, name, role)`, so its "dup keys" for those tables (4 and 16) are rows it never compared: a name with two types, an artist credited as both `AlbumArtist` and `Artist`. So every count below comes from direct `sqlite3` queries, and every credit and image row was checked by hand.

**Results:**
- **People.** 24 credited items (9 movies, two of them the versions of `Twin Horizon`; 6 episodes, 6 tracks, 2 albums and 1 book), 47 credit rows and 7 `Peoples` rows. All 47 credits were compared on `(ItemId, Name, PersonType, Role, ListOrder, SortOrder)` and all 7 people on `(Name, PersonType)`. J against F: identical, apart from the random people ids. J against A: identical rowid for rowid, in both tables.
- **ItemValues:** 7 rows on each side, with the same `(Type, Value, CleanValue)` on all 7: types 0 and 1 for the two artists, type 2 for `Adventure`, `Ambient` and `Synthetic manual genre`. `ItemValueId` differs on all 7: both sides use random version-4 GUIDs, uppercase. A kept J's 7 rows with J's ids.
- **ItemValuesMap:** J has 38 rows (Artist 8, AlbumArtist 8, Genre 22). F has 39, the one extra being the extra's `Adventure` (gl-1595). A has J's 38.
- **LinkedChildren:** the 11 rows are byte-identical in J and F: the same `ParentId`, `SortOrder`, `ChildId` and `ChildType`. They are 3 box-set rows and 7 playlist rows, all `Manual` (0), and one `LocalAlternateVersion` (2) for `Twin Horizon`'s 720p file. A matches J.
- **AncestorIds:** J has 156 rows and F 147. F's 147 are all in J. J's 9 extra rows are the four playlists' (the Playlists folder and the root, two each) and the Playlists folder's (the root): gl-1626. A has J's 156. The chains are identical on both sides for every kind the fixtures hold:

  | Kind | Ancestors | Rows each |
  |---|---|---|
  | Movie, Series, BoxSet, Book, PhotoAlbum, MusicArtist from a directory | location `Folder`, library, root | 3 |
  | Season, MusicAlbum, Photo | the above plus its series, artist or album | 4 |
  | Episode, Audio | the above plus its season or album | 5 |
  | Location `Folder` | root | 1 |
  | `CollectionFolder` | user root | 1 |
  | The `Video` extra | its library only | 1 |

  `AggregateFolder`, `UserRootFolder`, `Genre`, `MusicGenre`, `Person` and the PLACEHOLDER row have none on either side.
- **Padding:** none of the three databases holds an `ItemValues.Value` with surrounding spaces (`Value <> trim(Value)`: 0 rows), so gl-1635 is not visible in these runs.
- **BaseItemImageInfos:** J 44 rows, F 50, A 50. The 44 match on item and type. F and A add 6 Primary rows on audio tracks (gl-692). Every row's `Id` and `Blurhash` differ J against F; `Width` and `Height` agree on every row but one; `Path` differs on 17; `DateModified` on 20.
- **KeyframeData:** 15 rows, equal in every column in all three databases.
- **TrickplayInfos:** 16 rows, equal apart from `Bandwidth` (J 289 and F 285 on 15 rows; Bright Lattice J 299, F 288). A kept J's rows, 289 included.
- **MediaSegments:** 0 rows in J, F and A (the seed runs no segment provider). Its verdicts come from code reading.
- **Text forms:** image ids are uppercase hyphenated v4 GUIDs on both sides (version nibble 4, RFC variant, on all 44 J, 50 F and 50 A rows). Keyframe ticks are the same JSON array text (`[0,2500000000,5000000000]`). `PRAGMA integrity_check` is `ok` on A and on F.

## People: `Peoples` and `PeopleBaseItemMap`

Neither table has a date column, so the datetime text form (gl-1618) does not apply.

Ferrofin's writers:
- `update_people` (ppl:1925-1998). It is reached from:
  - the scan (`crates/ferrofin-core/src/library_scan.rs:8799-8801`, `:9132-9145`);
  - the provider refresh (`crates/ferrofin-providers/src/provider_manager.rs:2920-2924`);
  - the metadata editor (`crates/ferrofin-api/src/handlers/item_update.rs:99-104`, through `crates/ferrofin-core/src/library_manager.rs:3533-3549`, its `SupportsPeople` gate at `:3538-3543`).
- The Refresh People task's first phase (`crates/ferrofin-core/src/scheduled_tasks/library.rs:895-941`).
- The adoption repair `merge_peoples_rows` (`crates/ferrofin-core/src/adoption_repairs.rs:3320-3368`).
- Migrations `0007` (`:357-359`, fresh databases only) and `0042` (`:127`).
- The `ON DELETE CASCADE` from `BaseItems`: `delete_items` (ips:2314-2378) and the rekey fold `merge_one` (ips:4665).

Upstream's writers:
- `PeopleRepository.UpdatePeople` (PPL:112-252), reached through `LibraryManager.UpdatePeopleAsync` (LM:3736-3748). Its callers are:
  - `MetadataService.SaveItemAsync` (`MediaBrowser.Providers/Manager/MetadataService.cs:320-325`);
  - `AudioFileProber` (`MediaBrowser.Providers/MediaInfo/AudioFileProber.cs:341`);
  - `FFProbeVideoInfo` (`MediaBrowser.Providers/MediaInfo/FFProbeVideoInfo.cs:527`);
  - `AlbumMetadataService.SetPeople` (`MediaBrowser.Providers/Music/AlbumMetadataService.cs:195-227`);
  - `ItemUpdateController` (`Jellyfin.Api/Controllers/ItemUpdateController.cs:93-103`);
  - `ChannelManager` (`src/Jellyfin.LiveTv/Channels/ChannelManager.cs:1167`), for channel plugins, which Ferrofin does not load.
- `ItemPersistenceService.DeleteItem` (IPS:147, 153-154).
- `PeopleValidationTask` (`Emby.Server.Implementations/ScheduledTasks/Tasks/PeopleValidationTask.cs:114-168`).
- The migration `MergeDuplicatePeople` (`Jellyfin.Server/Migrations/Routines/20260508130000_MergeDuplicatePeople.cs:218-310`).

The credit-list builders were checked only as far as they set the values these tables store: the `PersonType` each of the NFO, TMDB, OMDb, TVDB, book, embedded-video, audio-tag and album builders writes (the `Peoples.PersonType` row), and whether each one goes through the `add_person` port (`crates/ferrofin-providers/src/container_types.rs:269`) or relies on `update_people`'s dedupe ("Input side"). Which credits each provider yields belongs to the provider issues cited there.

| Column | Ferrofin writer | Upstream writer | Verdict | Note |
|---|---|---|---|---|
| Peoples.Id | ppl:1119-1131: the stored row's id when one matches, else the credit's own id (the builders mint `Uuid::new_v4()`, e.g. `crates/ferrofin-core/src/library_scan.rs:23050`, `crates/ferrofin-api/src/handlers/item_update.rs:327`, `crates/ferrofin-providers/src/tmdb.rs:447`; the album builder leaves it empty, `crates/ferrofin-core/src/library_scan/music.rs:279-283`), else `new_v4` (ppl:1122-1125) | `Map(PersonInfo)` PPL:388-398 takes `PersonInfo.Id`, which its constructor sets to `Guid.NewGuid()` (`MediaBrowser.Controller/Entities/PersonInfo.cs:17-21`) | match | random v4, uppercase and hyphenated, on both sides: all 7 ids in J and in F have version nibble 4 and variant 8-B. The diff's "Id differs on 5" is the randomness |
| Peoples.Id (row reuse) | lookup `"Name" COLLATE NOCASE IN (…)` keyed on `to_ascii_lowercase` and type, lowest `Id` (ppl:1003-1010, 1096-1121) | `PersonType == type && names.Contains(e.Name.ToLower())`, with `names` lowered by `ToLowerInvariant` (PPL:171-186), then the first match (PPL:195-201) | owner? (P1) | the same for ASCII names. For a name with an uppercase non-ASCII letter (É, Ł, any Cyrillic or Greek capital), upstream's lookup never finds the stored row, because SQLite's built-in `lower()` folds ASCII only and Jellyfin registers no replacement (no `CreateFunction` in its tree). So it inserts another row with the same name each time it writes a changed credit list naming that person. An unchanged list returns before the lookup (`mappingsAreUnchanged`, PPL:140-161, the return at :158). Ferrofin reuses the row. Static only: no fixture name has a non-ASCII capital |
| Peoples.Name | trimmed (ppl:1098), bound at ppl:1143; the casing of the first credit per (name, type) (ppl:1972-1982) | `person.Name.Trim()` (PPL:116); `distinctPersons` keeps the first credit per (lowered name, type) (PPL:129); stored by `Map` (PPL:392) | match | J and F hold the same 7 names. A reused row keeps its stored casing on both sides |
| Peoples.PersonType | the builder's `PersonKind` name, always `Some`: editor `crates/ferrofin-api/src/handlers/item_update.rs:329-334` (`Unknown` when absent), NFO `crates/ferrofin-core/src/library_scan.rs:23052`, audio `:23622`, album `crates/ferrofin-core/src/library_scan/music.rs:279-282`, TMDB `crates/ferrofin-providers/src/tmdb.rs:449`, OMDb/TVDB `crates/ferrofin-providers/src/episode.rs:260,285`, book `library_scan.rs:19665` | `people.Type.ToString()` (PPL:393); the editor's absent `Type` is `PersonKind.Unknown` | match | `Actor`, `Author`, `AlbumArtist` and `Artist` in J and F. Neither side writes NULL in production (only a Ferrofin test does, ppl:4909) |
| PeopleBaseItemMap.ItemId | `insert_credit_rows` ppl:960-1000, `guid_to_db(item_id)` | PPL:227 | match | uppercase on both sides; J and F credit the same 24 item ids |
| PeopleBaseItemMap.PeopleId | the resolved row (ppl:1983, 1992, 987) | an unchanged credit keeps its existing mapping (PPL:214-221); a new one takes `entityPerson.Id` (PPL:229) | match; owner? edge (P2) | Ferrofin deletes the item's credits (ppl:1955) and re-inserts them, re-resolving each person. Where one name has duplicate `Peoples` rows, a kept credit can move to the lowest `Id`, while upstream leaves it on its row. With no duplicates the values are equal. Evidence for the write shape on the edited movie: J updated rows 28 and 29 in place, F wrote rows 48 and 49, and the values are equal |
| PeopleBaseItemMap.Role | `credit_role` trims and gives `''` when missing (ppl:872-874); a role differing from the stored one only in case keeps the stored casing (ppl:981-986, 1944-1947) | `Role?.Trim() ?? string.Empty` (PPL:117); a kept mapping's `Role` is not rewritten (PPL:214-221); a new one takes `credit.Person.Role` (PPL:232) | match; owner? edge (P2) | the case-only role rule is P2's second edge. J and F: `Lead` ×15, `Support` ×15, `''` ×17 (audio, album, book) |
| PeopleBaseItemMap.ListOrder | the credit's 0-based position after the dedupe (ppl:971, 989, 994) | `listOrder` over `distinctCredits` (PPL:209, 230, 236); the same index in the unchanged check (PPL:145) | match | 0 and 1 on every item in J and F |
| PeopleBaseItemMap.SortOrder | the credit's own order, NULL when it has none (ppl:990-993) | `credit.Person.SortOrder` (PPL:218, 231) | match | 0/1 from the NFO `<order>` on 28 rows; NULL on 19. The NULLs are audio, album and book, and the edited movie's 2, since the editor's `PersonInfo` carries none (`Jellyfin.Api/Controllers/ItemUpdateController.cs:96-101`; `item_update.rs:338`). Equal in J and F |
| all (credit dedupe) | `dedupe_people` ppl:854-860: first seen per (trimmed lowered name, type, lowered role) | `DistinctBy((LoweredName, PersonType, LoweredRole))` PPL:122-127 | match | (gl-432) |
| all (unchanged skip) | `credits_unchanged` ppl:896-921, applied at ppl:1939-1942 | `mappingsAreUnchanged` PPL:140-161 | match | the adoption rescan (A) left both tables identical to J, rowid for rowid. Ferrofin also rewrites identical values when a credited person has no `Person` item yet (ppl:1939-1941, `unchanged_people` returning `None`), part of P2's write shape |
| all (write gate) | the editor checks `supports_people` (`library_manager.rs:3538-3543`). The scan (`library_scan.rs:8799-8801` → `:9139`) and the provider refresh (`provider_manager.rs:2920-2924`) call the repository directly, without it | `result.Item.SupportsPeople && result.People is not null` (`MediaBrowser.Providers/Manager/MetadataService.cs:320`), and again in `UpdatePeopleAsync` (LM:3738-3741) | fix (verify first): gl-1634 | every fixture item that carries credits supports them. The kinds without support are listed in `crates/ferrofin-core/src/kinds.rs:110-122`; upstream's kinds with support are `Video` and its subclasses, `Audio`, `MusicAlbum`, `Book`, `BoxSet`, `Series` and `Season`, which the list matches. An independent code check found the scan reachable for a `MusicArtist` whose `artist.nfo` holds `<actor>`, `<director>`, `<writer>` or `<credits>`: `nfo_kind_of` (`library_scan.rs:21355, 21384`) → `fetch_music` (`:11407`) → the plain parser's `add_person` (`crates/ferrofin-providers/src/xbmc.rs:71-82`, `crates/ferrofin-providers/src/xbmc/base_parser.rs:365-391`) → `nfo_people` `:8162`, `local_people` `:8274`, `pass_credits` `:8356` → `persist_people` `:8800`. Kodi's artist schema has none of these tags and Jellyfin's saver never writes them for an artist, so it is rare. The provider refresh pushes credits only from its season and episode arms (`provider_manager.rs:2194, 2324`), so that call is latent. `crates/ferrofin-traits/src/library.rs:823` describes the write as gated |
| all (editor, nameless person) | `PersonRequest::entity` drops an entry with a null, empty or blank name (`item_update.rs:318-325`) | `person.Name.Trim()` (PPL:116) throws on a null name, failing the request, and stores an empty or blank one as a credit named `''` (read from the code, not run) | owner? (P3) | the null case is documented in the code (`item_update.rs:318-320`). The empty and blank cases are not |

**Deletion.**

| What removes rows | Ferrofin | Upstream | Verdict |
|---|---|---|---|
| a dropped credit's `Peoples` row | the credit row goes (ppl:1955). The person row stays until Refresh People's orphan sweep (`scheduled_tasks/library.rs:934-940`), weekly by default (`:1109-1110`) | `DeleteCreditsWithoutMapping(context, droppedCredits)` in the same transaction (PPL:239-248, 263-275) | fix (gl-464, open, still present at `978bea505`: ppl:1925-1998 has no `DELETE FROM "Peoples"`) |
| item deletion | the credits cascade with `BaseItems` (`delete_items`, ips:2314-2378); the `Peoples` rows are left for the weekly sweep | reads the closure's `PeopleId`s, deletes the credits, then the `Peoples` rows with none left (IPS:147, 153-154) | fix (gl-439, open, still present). The rekey fold `merge_one` (ips:4665, recorded under D4 and D9b) drops the loser's credits by the same cascade, with the same gap. A fix for gl-439 should cover it |
| Refresh People, phase 1 | merges `(Name, PersonType)` duplicates with `UPDATE OR IGNORE`, drops the colliding remainder, then removes the people no credit maps (`scheduled_tasks/library.rs:895-941`) | `ExecuteUpdate` re-points the duplicates' credits, then deletes them and the orphans (`Emby.Server.Implementations/ScheduledTasks/Tasks/PeopleValidationTask.cs:114-168`). A duplicate credit on the same `(ItemId, Role)` hits the primary key and the task throws (read from the primary key, not run) | match; owner? edge (P4; documented: `scheduled_tasks/library.rs:911-913`) |
| case-only duplicate `Peoples` | `merge_peoples_rows` (`adoption_repairs.rs:3320-3368`): grouped by lowered name and type; the keeper has the most credits, ties going to the lowest `Id`; colliding credits are dropped, the rest re-pointed | `MergePeoplesRowsAsync` (`Jellyfin.Server/Migrations/Routines/20260508130000_MergeDuplicatePeople.cs:218-310`), with the same grouping, keeper and order | match |
| orphaned parent chains | migration `0042` deletes the orphans' credits by hand (`:127`), since Ferrofin's migrations run with foreign keys off | the cascade of 12.1's `ChangeOwnerIdToGuid` orphan block | match (same end state; neither deletes `Peoples` rows there) |

**Rows only one side writes.**
- **In the runs:** none. J and F hold the same 7 `Peoples` and 47 credit rows, and A holds J's rows unchanged.
- **Static:**
  - **Non-ASCII names.** Upstream inserts extra same-name `Peoples` rows for a name with a non-ASCII capital (the row-reuse row, P1), on each write that changes the item's credit list. An unchanged list writes nothing (PPL:158). The Refresh People dedupe merges them later (`Emby.Server.Implementations/ScheduledTasks/Tasks/PeopleValidationTask.cs:114-157`). Until then they appear as one orphan per changed write of an item already credited, and as one row per newly credited item.
  - **Kinds without credit support.** Ferrofin may write credits for an item kind upstream refuses them for (the write-gate row, gl-1634).

**Input side (which credits are written).** Each of the eight builders was checked for which path its credits take:
- **Through the `add_person` port** (`crates/ferrofin-providers/src/container_types.rs:269`): the NFO parser.
- **Relying on `update_people`'s dedupe, with no role:** the embedded-video, audio-tag, album, book and OMDb builders (the book's is `book_people`, `crates/ferrofin-core/src/library_scan.rs:19659-19672`; OMDb's is `omdb_people`, `crates/ferrofin-providers/src/episode.rs:254-274`, `role: None` at `:268`, as upstream's `OmdbProvider.AddPeople` sets only `Name` and `Type`, `MediaBrowser.Providers/Plugins/Omdb/OmdbProvider.cs:485-489`). Their credits carry no role, and none is a `GuestStar`; the embedded-video credits (roleless `Actor`s from `WM/MediaCredits`, upstream `MediaBrowser.MediaEncoding/Probing/ProbeResultNormalizer.cs:1652-1654`, and roleless writers, producers and directors from `ProcessPairs`, `:542-588`) and the audiobook performers are credits without a role. So `AddPerson`'s extra rules change nothing for them: the role fill, the roleless merge, and Actor and GuestStar counted as one credit. Upstream's audio prober calls `PeopleHelper.AddPerson` with no `Role` (`MediaBrowser.Providers/MediaInfo/AudioFileProber.cs:290-341`).
- **Relying on `update_people`'s dedupe, with roles:** TMDB (`people_entities`, `crates/ferrofin-providers/src/tmdb.rs:441-456`) and TVDB (`tvdb_people`, `crates/ferrofin-providers/src/episode.rs:279-292`). Upstream's providers call `AddPerson` (for TMDB, `MediaBrowser.Providers/Plugins/Tmdb/Movies/TmdbMovieProvider.cs:312, 359` and `MediaBrowser.Providers/Plugins/Tmdb/TV/TmdbEpisodeProvider.cs:242, 273, 314`), and these credits carry roles, so its rules can apply. For TVDB that is tracked by gl-1125 (TVDB credits skip `AddPerson`'s rules). For TMDB it is F11, gl-1651: **fix (verify first)**. All four TMDB builders (movie, series, season and episode, through `people_entities`) skip the rules, so a person in both `cast` and `guest_stars` with one character gives two credits where upstream keeps one `GuestStar`, and a roleless repeat of a person is kept where upstream merges it. No fixture reaches it (the runs block providers).

## ItemValues

Ferrofin's writers:
- `write_item_values` and `item_value_ids` (ips:196-352), fed by `item_values_of` (`crates/ferrofin-db/src/entities/base_items.rs:548-571`), which splits the item row's five `|`-joined columns;
- `ensure_by_name_item` (ips:2873-2925);
- `refresh_clean_values` (ips:1177-1222) and migration `0031`;
- the prune after a deletion (ips:2369-2378, `PRUNE_ORPHAN_ITEM_VALUES_SQL` at :6713).

Upstream's: IPS:364-395, `GetItemValuesToSave` (IPS:792-815), the prune in `DeleteItem` (IPS:149), and `Jellyfin.Server/Migrations/Routines/20260610120000_RefreshCleanNamesAndValues.cs:109`.

| Column | Ferrofin writer | Upstream writer | Verdict | Note |
|---|---|---|---|---|
| ItemValueId | ips:210 `Uuid::new_v4()` on the first write of a `(Type, Value)`; `ensure_by_name_item` ips:2906-2913 | IPS:384 `Guid.NewGuid()` | match; owned edge (gl-1597, gl-1624) | random v4 and uppercase on both sides (J, F). A kept J's ids. Ferrofin reuses this id as the `BaseItems.Id` of its Genre and Studio rows (ips:282-317), and `ensure_by_name_item` writes a value row that no item maps to: owned by gl-1597 (gl-1624 for by-name artists) |
| Type | 0 to 4 from `item_values_of` (`base_items.rs:561-569`) | 0 to 4 and `InheritedTags` (6) (IPS:796-810) | match; owner? edge (P5) | the edge is type 6, which Ferrofin never writes. J and F agree on types 0, 1 and 2. Studios (3) and Tags (4) are read in code only; no fixture has them |
| Value | the `\|` segment, **trimmed** (`base_items.rs:553`; the test at `:582-602` asserts the trim) | the item's list entry as it is; only blank entries are dropped (IPS:812 `RemoveAll(IsNullOrWhiteSpace)`, `Value = f.Value` at :386) | fix (gl-1635) | gl-1588's live check (its note on gl-1560): Genres `["Drama"," drama "]` gives Ferrofin `(2,'Drama')`, `(2,'drama')` where Jellyfin writes `(2,'Drama')`, `(2,' drama ')`. The blank rule matches: both drop whitespace-only entries. Effects under the ItemValuesMap table |
| CleanValue | `get_clean_value` (`crates/ferrofin-util/src/string_extensions.rs:266-273`), on insert (ips:271) and in `refresh_clean_values` | `GetCleanValue` (IPS:383; `src/Jellyfin.Extensions/StringExtensions.cs:159-176`) | match | equal on all 7 rows in J, F and A; the same algorithm. Upstream's 12.0 refresh routine is ported (`refresh_clean_values`) |
| (row lifecycle) | rows are only added; a value no item maps to any more is deleted with each item deletion (ips:2374) | the same: added (IPS:388), pruned in `DeleteItem` (IPS:149) | match | neither side prunes a value that a save unlinks. J, F and A hold no unmapped value row |

## ItemValuesMap

Ferrofin's writer is `write_item_values` (ips:244-352). It deletes the item's links (ips:261-265), then inserts one per distinct `(Type, Value)` (ips:266-272, 327-337). Its callers:
- the scan (`crates/ferrofin-core/src/library_scan.rs:9179-9184`);
- the metadata editor's batch (`crates/ferrofin-core/src/library_manager.rs:3232-3241`);
- the provider refresh (`crates/ferrofin-providers/src/provider_manager.rs:2901`);
- the music pass (`crates/ferrofin-core/src/library_scan/music.rs:1233-1273`);
- Live TV (`crates/ferrofin-livetv/src/manager.rs:1916`);
- the extras copy (ips:7076-7090).

Upstream, on every save, keeps the item's links that still match, adds the missing ones and removes the rest (IPS:397-422).

| Column | Ferrofin writer | Upstream writer | Verdict | Note |
|---|---|---|---|---|
| ItemId, ItemValueId | as above | IPS:397-422 | match | J 38 = A 38; F's 38 others equal J's. Dedupe is exact `(Type, Value)` on both sides (ips:266-272; IPS:369, 394) |
| (the extra's genre) | the extras copy (ips:7076-7090) | none on a first scan | owned (gl-1595) | F maps the extra `Synthetic making of` to `Adventure`, the genre of its owner `Amber Harbor`. Ferrofin copies the owner's metadata after the owner's refresh, and Jellyfin before it, so Jellyfin's first scan copies nothing: J's extra has `Genres` `''` and no map. This is the BaseItems ledger's `Genres` row. A's rescan left the extra as Jellyfin wrote it |
| (an item left with no values) | the scan writes nothing when the item's list is empty (`library_scan.rs:9179-9184`, from `scan_item` at `:8791`), so the links it had stay | the save removes them (IPS:421) | fix (verify first): gl-1636 | the other five callers pass the empty list through, which clears the links, and the test at ips:9091-9125 asserts that it does. An independent code check narrowed the path: a Default rescan keeps the stored values (`keep_existing`, `crates/ferrofin-providers/src/metadata_merge.rs:211, 238`, as upstream's `MediaBrowser.Providers/Manager/MetadataService.cs:902-905` does), and an audio track keeps its stored artists (`library_scan.rs:22621-22631, 22669-22676`). It happens on "Replace all metadata" when no reader or provider gives back any value: the five columns are cleared (`metadata_merge.rs:634`), the empty list skips the write, and the genre, studio and tag links stay, read by the browse counts, the filters and the parental tag rules |
| (padded entries) | `Drama` and ` Drama ` collapse onto one link | two values, two links | fix (gl-1635) | the same issue as `ItemValues.Value` |
| (InheritedTags links) | none | written on every save; all deleted at the end of each scan's post-scan tasks (LM:1597 → IPS:161-170) | owner? (P5) | |
| (deletion) | `ON DELETE CASCADE` with the item row (ips:2369-2373), then the value prune | explicit delete (IPS:144), then the prune | match | |

**The trim's effects** (gl-1635):
- An entry that differs from another only by padding collapses onto it: one link where Jellyfin writes two.
- On an adopted database, Ferrofin's next save of an item carrying a padded value links it to a new trimmed row. Jellyfin's padded row is left unmapped until a deletion prunes it (static).
- After gl-1588 the editor keeps the padding in `BaseItems`, so this trim is what still differs.

## LinkedChildren

Ferrofin's writers, by what they write:
- **Box sets:**
  - `create_collection` and `add_to_collection` through `upsert_linked_child` (`crates/ferrofin-core/src/collection_manager.rs:768-772, 813-817`; `crates/ferrofin-core/src/linked_children_service.rs:231-273`);
  - `remove_from_collection` (`collection_manager.rs:834-852`).
- **Playlists:**
  - `write_entries_in` on every add, remove or move (`collection_manager.rs:2063-2089`);
  - the `.m3u` scan's `write_scanned_members` (ips:5485-5540).
- **Versions:**
  - the scan's `apply_local_version_writes` (ips:5572-5612);
  - merge and split through `set_primary_version_id` (ips:2462-2519);
  - `promote_version` (ips:3563-3660), `drop_version` (ips:3671-3715) and `release_owned_versions` (ips:3436-3458).
- **Re-routing:** `reroute_linked_children` (`linked_children_service.rs:175-229`).
- **Deletion:** ips:2343-2358.
- **Adoption:** `import_membership`, the port of `MigrateLinkedChildren` (`crates/ferrofin-core/src/item_data.rs:1130-1188`); migration `0034`.

Upstream's: IPS:490-790 on every save of a folder or video, `Jellyfin.Server.Implementations/Item/LinkedChildrenService.cs:137-201`, `DeleteItem` (IPS:145-146), and the migration routines under "Upstream migration routines".

| Column | Ferrofin writer | Upstream writer | Verdict | Note |
|---|---|---|---|---|
| ParentId, ChildId | as above | as above | match | 11/11 rows identical in J and F. Re-routing reaches the same end state on both sides: a link whose parent already lists the target is dropped, the rest are re-aimed (`Jellyfin.Server.Implementations/Item/LinkedChildrenService.cs:137-170`) |
| ChildType | 0 for box-set and playlist entries (`collection_manager.rs:75`); 2 and 3 for versions (ips:5175-5179) | `LinkedChildType` 0 to 3 (`src/Jellyfin.Database/Jellyfin.Database.Implementations/Entities/LinkedChildType.cs`) | match | the same discriminants; `Manual` on 10 rows and `LocalAlternateVersion` on 1, on both sides. **`Shortcut` (1), reachability.** Upstream's only writer is `Folder.RefreshLinkedChildren` (`MediaBrowser.Controller/Entities/Folder.cs:1897-1945`; `CollectionFolder` overrides it, `MediaBrowser.Controller/Entities/CollectionFolder.cs:268-299`, to copy its physical folders' entries, and creates no `Shortcut` entry itself), which `RefreshedOwnedItems` (`:1875-1890`) calls on every metadata refresh of a folder (`MediaBrowser.Controller/Entities/BaseItem.cs:1547-1553`). It acts only where `SupportsShortcutChildren` is true, which only `UserRootFolder` sets (`MediaBrowser.Controller/Entities/UserRootFolder.cs:42`; the default is false, `MediaBrowser.Controller/Entities/Folder.cs:151`). So it reads only a `.mblink` file lying directly in `root/default`, the user root's directory. Neither server writes one there: both put a library's shortcuts in the library's own folder, `root/default/<library>/` (LM:3972-3977, 4147-4162; `crates/ferrofin-core/src/virtual_folder_manager.rs:1445-1460`, called with the library's folder at `:1837`, `:2008` and `:2074`), and J's `root/default` holds only the seven library folders. Only a file placed there by hand reaches it. Ferrofin has no port of it and no other writer of 1 in `crates/`: its adoption import keeps a stored `Shortcut` entry of a 10.x `Data` blob as 1 (`crates/ferrofin-core/src/item_data.rs:73-78, 1150-1153`), as `MigrateLinkedChildren` does (`Jellyfin.Server/Migrations/Routines/20260113120000_MigrateLinkedChildren.cs:147-168`). None of J, F and A holds a 1. Unreachable through either server's own library setup, so the verdict stays |
| SortOrder | dense from 0 on every playlist write and on the scan's version rows; `MAX + 1` on an upsert (`linked_children_service.rs:258-263`) and on a merge (ips:2503-2515) | dense from 0 on every save of the parent (IPS:623-648, 702-724), a box-set add included, which reassigns the list and saves (`Emby.Server.Implementations/Collections/CollectionManager.cs:259-273`); `MAX + 1` in `UpsertLinkedChild` (`Jellyfin.Server.Implementations/Item/LinkedChildrenService.cs:183-185`), which only `MediaBrowser.Controller/Entities/Video.cs:618` calls | match; not compared for `promote_version`'s row order or a merge whose primary was itself a version | J and F equal on all 11 rows. Ferrofin's box-set add appends at `MAX + 1`, citing `UpsertLinkedChild`; that gives the same values while the rows have no gap |
| SortOrder (a box-set removal, a deleted merged version) | one `DELETE`, no renumbering (`remove_from_collection` `collection_manager.rs:834-859`, the delete at `:841-849`; `drop_version` ips:3671-3715, the delete at `:3679-3688`) | the parent is saved with its new list and renumbered from 0 (`Emby.Server.Implementations/Collections/CollectionManager.cs:318-321`; LM:539-552, the primary saved at `:550`) | fix (verify first): gl-1637 | the order is kept, so no client sees it, and no reader needs dense values; the stored values differ until Jellyfin saves the parent. An independent code check confirmed both paths. A box set's gaps are permanent: nothing else rewrites its rows. A deleted version (`DELETE /Items/{id}` through `delete_item` → `detach_alternate_version`, `library_manager.rs:3354, 2448-2460`) leaves a gap until the next scan of the primary's folder, which renumbers (`renumbered_version_rows` ips:5436-5455 → `apply_local_version_writes`) |
| SortOrder (dedupe and repeats) | playlists keep repeats; a box set keeps one row per child, at its first position; the migration keeps the original ordinals after its dedupe (`item_data.rs:1150-1168`) | the same (IPS:604-613; `Jellyfin.Server/Migrations/Routines/20260113120000_MigrateLinkedChildren.cs:132-172, 205-221`) | match | |
| (deletion) | both directions (ips:2345-2346), no renumbering | the same (IPS:145-146) | match | gaps on both sides. Upstream's next save of a box set closes them; Ferrofin's box sets keep them (gl-1637) |

## AncestorIds

Ferrofin's writers:
- **The scan:** `set_ancestors` (ips:3014-3051), from the planned closure, for a new item or one whose closure changed (`crates/ferrofin-core/src/library_scan.rs:7962-7966, 8794-8797`; `crates/ferrofin-core/src/library_scan/missing_episodes.rs:601, 720, 802`).
- **Box sets:** `add_ancestors_in` (`crates/ferrofin-core/src/library_structure_repository.rs:271-287`, from `collection_manager.rs:749`).
- **Libraries:** the `CollectionFolder` under the user root (`crates/ferrofin-core/src/virtual_folder_manager.rs:724, 756`).
- **Location folders:** `library_structure_repository.rs:130, 171`.
- **Repairs:**
  - `merge_into_location` (`library_structure_repository.rs:313-366`);
  - `rekey_container` (ips:789-792);
  - the user-view consolidation (`crates/ferrofin-core/src/user_view_repository.rs:433-480`);
  - `crates/ferrofin-core/src/adoption_repairs.rs:3029-3035, 3288-3290`;
  - migrations `0042` and `0044`.
- **Deletion:** `ON DELETE CASCADE` on both foreign keys (schema `AncestorIds`).

Upstream's writer is IPS:321-322, 424-477 on every save. It writes `GetAncestorIds()` (`MediaBrowser.Controller/Entities/BaseItem.cs:2943-2946`, the parents plus the collection folders), keeps only ancestors that exist (IPS:443-455), and removes the rest. `DeleteItem` deletes them explicitly (IPS:134-135).

| Column | Ferrofin writer | Upstream writer | Verdict | Note |
|---|---|---|---|---|
| ItemId, ParentItemId | as above | as above | match | 147 rows identical for every kind the fixtures hold (the chains under Method) |
| (playlists and the Playlists folder) | no writer | written on create and every save | owned (gl-1561: [`jellyfin-parity-playlist-ancestors.md`](jellyfin-parity-playlist-ancestors.md), PR 931; fix gl-1626) | J's 9 rows F lacks |
| (Live TV channels) | none: the projection sets the channel's `ParentId` to the Live TV view (`channel_entity`, `crates/ferrofin-livetv/src/projection.rs:306-345`, at `:316` and `:341-342`), channels are saved through `sync_channel_items` (`crates/ferrofin-livetv/src/manager.rs:1608-1648`) → `save_items` (ips:2431) → `upsert_item` (ips:1757-1809), which write no ancestors, and nothing in `ferrofin-livetv` writes `AncestorIds` (the one `set_ancestors` call, `manager.rs:12774`, is a test helper) | `ParentId` is the Live TV folder (`src/Jellyfin.LiveTv/Guide/GuideManager.cs:422-427`), and `LiveTvChannel` keeps `SupportsAncestors` | fix (verify first): gl-1638 | no fixture configures a tuner. An independent code check confirmed it: upstream writes exactly one row per channel, `(channel, the Live TV view)`. The view (`src/Jellyfin.LiveTv/LiveTvManager.cs:1271-1275`, created without a parent, LM:3002-3051) has no ancestors and matches no collection folder (LM:2841-2887). Recursive queries and the tag rules scope Live TV by `TopParentId` on both sides; the missing rows change the descendant-aware folder filters (`isPlayed`, `IsResumable`, `OfficialRatings`, …) on the Live TV view row itself |
| (kinds without ancestors) | none for Genre, MusicGenre, Studio, Person, Year and LiveTvProgram | `SupportsAncestors => false` for exactly these (`MediaBrowser.Controller/Entities/Genre.cs:32`, `MediaBrowser.Controller/Entities/Person.cs:39`, `MediaBrowser.Controller/Entities/Studio.cs:31`, `MediaBrowser.Controller/Entities/Year.cs:20`, `MediaBrowser.Controller/Entities/Audio/MusicGenre.cs:24`, `MediaBrowser.Controller/LiveTv/LiveTvProgram.cs:163`) | match | none on either side for Genre, MusicGenre and Person |
| (an ancestor with no row) | `set_ancestors` (ips:3014-3051) writes the closure in one transaction with `INSERT OR IGNORE`, which ignores a duplicate but not a foreign-key failure, and foreign keys are on (`crates/ferrofin-db/src/database.rs:168`). So one ancestor with no `BaseItems` row fails the statement: the transaction rolls back, its `DELETE` included, so the item keeps its previous closure, and the error goes to the caller. `add_ancestors_in` (`library_structure_repository.rs:268-287`) skips such an ancestor, inserting through `SELECT … FROM "BaseItems"` | keeps only the ancestors that exist and skips the rest (IPS:443-455) | match | unreachable outside a race. Every production caller passes ancestors that have rows. **The scan:** the closure is the stored one or the plan's; parents are planned before their children, and a new item that leaves no row takes everything planned below it along (`library_scan.rs:7018-7029`; the test `a_new_series_that_fails_takes_its_children_along`, `crates/ferrofin-core/tests/library_scan_item_isolation.rs:805`). **A library:** its closure is the user root, created first by `root.ensure()` (`virtual_folder_manager.rs:716-724, 738-756`). **A virtual season or episode:** the series' stored closure (`crates/ferrofin-core/src/library_scan/missing_episodes.rs:495-506`) plus the series and a season saved just before it (`:716-720`, `:799-802`). **Never passed:** a user-view id or the Live TV view. Live TV calls `set_ancestors` only from a test, and the user-view repairs write their own SQL (`user_view_repository.rs:433-480`). **The race:** an ancestor deleted between the plan and the save, by a concurrent `DELETE /Items/{id}`, would fail it. The scan then counts the item skipped and clears its `DateLastRefreshed` for the next scan (`library_scan.rs:7128-7146`), where upstream saves the item without that ancestor. Read from the code, not run |
| (deletion) | cascade | explicit deletes (IPS:134-135) | match | |
| (dangling rows on an adopted database) | migration `0044` deletes them (`crates/ferrofin-db/migrations/0044_dangling_user_and_ancestor_cleanup.sql:60-63`) | Jellyfin keeps them | recorded | owner decision on the adoption-realism milestone, option (a) (`0044:14-21`) |

## BaseItemImageInfos

Ferrofin's writers, all in `crates/ferrofin-core/src/` unless named:
- `item_persistence_service.rs` (ips):
  - `write_item_images` `:6893-6966`, behind `save_item_images` `:3072` and `save_items_images` `:3119`;
  - `add_item_image_if_absent` `:3090-3117`;
  - `set_item_image` `:4141-4174`, `set_item_image_at_index` `:4176-4231`, `delete_item_image` `:4233-4262`;
  - the local-version copy `write_owned_relations` `:7472-7525`;
- `item_repository.rs:4324-4398` (`swap_item_images`);
- `dynamic_images.rs` (di):
  - collages and single copies, `refresh_item` `:574-662`;
  - `fill_image_metadata` `:945-956`;
- `library_scan.rs`:
  - library tiles, `refresh_library_image` `:14148-14200`;
  - the scan's `fill_image_metadata` `:10877-10898`;
- `crates/ferrofin-providers/src/provider_manager.rs`: uploads and chosen images, `:3346`, `image_file_stem` `:4699-4705`.

Upstream:
- `BaseItemMapper.cs` (Map): `:390-393`, `MapImageToEntity` `:450-463`;
- `ItemPersistenceService.cs` (IPS): `SaveImagesAsync` `:179-218`, the update path `:350-353` and `:481-485`;
- `LibraryManager.cs` (LM): `UpdateImagesAsync` `:2556-2657`, `RunMetadataSavers` `:2795-2803`;
- `MediaBrowser.Providers/Manager/ImageSaver.cs`;
- `Emby.Server.Implementations/Images/BaseDynamicImageProvider.cs` (BDIP) and its subclasses.

**Datetime text form.** `DateModified` is the only datetime column of the 45. It goes through `datetime_to_db` (`crates/ferrofin-db/src/store.rs:41-47`), which always writes seven fractional digits. EF trims them: J holds `2026-11-11 04:29:14.286357` where F holds `2026-11-11 04:29:14.2863570`, the same file time. Three matched media-file rows differ only that way. This is the formatter gl-1565 investigated ([`jellyfin-parity-datetime-text.md`](jellyfin-parity-datetime-text.md), PR 920), fixed by gl-1618 (PR 923, open), cited as "datetime form" below. A row Ferrofin keeps is not rewritten: `write_item_images` leaves a row whose values match untouched (ips:6908-6916, `image_row_matches` at `:36-46`), so A kept J's EF text on every image row it did not change.

| Column | Ferrofin writer | Upstream writer | Verdict | Note |
|---|---|---|---|---|
| Id | `Uuid::new_v4()` for a new row (ips:6933, 3104, 4162, 4218, 7511); a row that matches, or the same type and path, keeps its id (ips:6907-6935) | `Guid.NewGuid()` on every save (Map:455); every save deletes and re-inserts the item's rows (IPS:183-197, :481-485) | match; owner? edge (documented, P6); owned edge (gl-1564: the album rows and the playlist collages) | random v4 on both, uppercase in all three databases. The P6 edge: Ferrofin keeps ids across saves and reads by `rowid`; upstream redraws them and reads by `Id`. The comment at ips:6918-6923 explains it (gl-65). A kept 42 of J's 44 ids; it re-ided only the two albums. The collages' new `Id` after a Jellyfin save is gl-1564's ([`jellyfin-parity-album-covers.md`](jellyfin-parity-album-covers.md)): Jellyfin's own row rewrite |
| Blurhash | scan `library_scan.rs:10894`; generated di:951; uploads filled by the next scan; swap clears it (`item_repository.rs:4381-4392`) | LM:2630-2639 on every image save | recorded; edge fix (gl-1054); owned edge (gl-1564: the album rows and the playlist collages) | hash strings differ on all 44 rows: recorded as the Jellyfin bug not ported "BlurHashSharp's blur-hash input" (owner, 2026-10-31, gl-1058). The collages' re-measure on a Jellyfin save is gl-1564's. Edges: an unreadable image stores NULL where upstream stores an empty blob (`GetImageBlurHash` returns `""` for a zero size, `src/Jellyfin.Drawing/ImageProcessor.cs:415-420`, and the catch at LM:2635-2639). A swap leaves NULL until the next scan, where upstream re-measures at once. Uploads: gl-1054 |
| DateModified | the file's mtime (`file_date_modified`, di:649; `crates/ferrofin-providers/src/local_images/merge.rs:24`); remote rows take chrono's default (`crates/ferrofin-livetv/src/projection.rs:417-423`) | the file's mtime (LM:2641-2649; `MediaBrowser.Controller/Entities/BaseItem.cs:2618-2626`); a remote row keeps `default(DateTime)` (`MediaBrowser.Controller/Entities/BaseItemExtensions.cs:45-55`, `src/Jellyfin.LiveTv/Guide/GuideManager.cs:719-741`) | fix (gl-1618; remote rows, verify first: gl-1639) | datetime form. Media-file values are equal. The 17 generated, uploaded and album rows differ by run time. A remote image row (Live TV artwork) would store `1970-01-01 00:00:00.0000000` where upstream stores `0001-01-01 00:00:00`: code read only, since no fixture has a remote image. An independent code check confirmed it for programmes: only `remote_image` (`projection.rs:417-423`, from `channel_images_update` `:454-475` and `program_images_update` `:526-556`) builds such a row; every other writer dates a local file. Upstream keeps `MinValue` on a programme whose artwork stays remote: `PreCacheImages` (`src/Jellyfin.LiveTv/Guide/GuideManager.cs:750`) makes local only the programmes ending within `MaxCacheDays` = 2 (`:47`, `:231`), and `UpdateImagesAsync` skips programmes (LM:2564-2575). A channel's logo upstream is downloaded into a local file dated by its time (P8). Readers also map a NULL date to the epoch (`crates/ferrofin-core/src/item_repository.rs:3013-3017`, ips:3178-3181, :3212-3215, `crates/ferrofin-core/src/dto_service.rs:411`). gl-1618's migration trims the text to `1970-01-01 00:00:00`, still the wrong date |
| Height | as Width | as Width | fix (gl-950); owned edge (gl-692: the track rows); edge fix (gl-1054) | equal on 43 of 44 rows. The `AggregateFolder` root: J 150, F 100. Both copy one image (`BaseFolderImageProvider`), but from different sources. J copied the orchard's uploaded PNG (100x150; the same blurhash `d309gm…`). F copied a track's album cover (100x100; blurhash `e0E*K%…`). That track is first by sort name only because of the extra track Primary rows (gl-692). F drew the root at the end of its main scan (04:36:35.57), before the orchard edit (04:36:39.24). J's first root image came when its last validation started (04:29:36.999). With the track rows gone, F would still pick another source (the first episode, 160x90), so the moment matters on its own: gl-950 (below) |
| ImageType | `image_type_to_disc` (`item_repository.rs:2971`), the `json_value` of `ImageType` | `(ImageInfoImageType)e.Type` (Map:461), the same 0-12 values | match | |
| ItemId | every writer | Map:454 | match; owned edge (gl-692) | the 6 extra track rows in F and A are gl-692's owner question. gl-1564's scope leaves the track-level cover to gl-692 |
| Path | media-file art: the file's path. Ferrofin's own art goes to `{metadata}/library/{ID}/primary.<ext>`: collages di:629, single copies di:1027-1044, library tiles `library_scan.rs:14185-14186`, uploads `provider_manager.rs:3346` with stem `:4699-4705` | `{metadata}/library/{xx}/{idN}/` (`MediaBrowser.Controller/Entities/BaseItem.cs:944-961`) with `poster`, or `folder` for a photo album, music album or artist, or person (`MediaBrowser.Providers/Manager/ImageSaver.cs:502-509, 562`) | fix (gl-1575); owned edge (gl-1564: the album rows); owner? edge (documented, P8) | 17 rows differ, split between two issues. **gl-1575:** the 15 Ferrofin art rows (4 library tiles, the root, the user root, 2 genres, the music genre, the 4 API-made playlists' collages, the photo album, the orchard upload); gl-1575's own evidence names the API-made playlist's collage. **gl-1564:** the 2 album rows ([`jellyfin-parity-album-covers.md`](jellyfin-parity-album-covers.md); its fix is gl-1633). gl-1564's write-up leaves the playlist collage's folder to gl-1575. Example: J `/config/metadata/library/be/becdaa9a…/poster.png`, F `/config/metadata/library/BECDAA9A-5190-2A2F-8EA0-7296DCF217E3/primary.png`. The photo album: J `…/b9/b9b19274…/folder.jpg`, F `…/B9B19274-…/primary.jpg`. Both store the resolved path, not `%MetadataPath%` (Map:456; J's rows). Media-file paths are equal. A Live TV channel's logo stays at its URL in Ferrofin, where upstream downloads it into a local file on every refresh: P8 |
| Width | scan `library_scan.rs:10877-10898`; generated di:945-956; swap sets 0 (`item_repository.rs:4381-4392`) | LM:2556-2657, forced on every `ImageUpdate` save (LM:2802) | match; edge fix (gl-1054) | equal on all 44 rows, the root included (100 on both). Edges: a swap stores 0 until the next scan, where upstream re-measures at once. An unreadable changed file keeps its old size, where upstream stores 0 (LM:2622-2628) |

**Rows only one side writes.** F and A each hold six track Primary rows that point at the album covers. J has none (gl-692). No row is J-only.

**Deletion.** Both sides delete an item's images with it. Ferrofin relies on the foreign key's `ON DELETE CASCADE`. Upstream does the same and also deletes them explicitly (IPS:137). Match.

## KeyframeData

Ferrofin: `crates/ferrofin-core/src/keyframe_repository.rs:60-92`, written only by the extractor cache (`crates/ferrofin-core/src/keyframe_extractors.rs:207-238`). Upstream: `Jellyfin.Server.Implementations/Item/KeyframeRepository.cs:55-75`, written by `src/Jellyfin.MediaEncoding.Hls/Cache/CacheDecorator.cs:43-60`. Both delete and re-insert in one transaction.

| Column | Ferrofin writer | Upstream writer | Verdict | Note |
|---|---|---|---|---|
| ItemId | `keyframe_repository.rs:72-77` | `Jellyfin.Server.Implementations/Item/KeyframeRepository.cs:38-43` | match | 15/15 |
| TotalDuration | as above | as above | match | 15/15 |
| KeyframeTicks | `serde_json::to_string` (`keyframe_extractors.rs:225-231`) | EF's JSON primitive collection | match | the same text in all 15 rows. Ferrofin treats a stored row without ticks as absent and extracts again (`keyframe_extractors.rs:170-197`). Upstream never stores one (`src/Jellyfin.MediaEncoding.Hls/Extractors/FfProbeKeyframeExtractor.cs:47` and `src/Jellyfin.MediaEncoding.Hls/Extractors/MatroskaKeyframeExtractor.cs:38` return false on an empty list), so a Jellyfin-written row is always served |

## MediaSegments

Ferrofin:
- msm: `run_provider` `:358-417`, `run_segment_providers` `:823-889`, `create_stored` `:325-349`, `clear_stored` `:350-357`, `delete_stored` `:283-294`, `delete_segments` `:527-536`;
- `crates/ferrofin-core/src/media_segment_repository.rs:22-74`;
- the only production caller: `crates/ferrofin-core/src/scheduled_tasks/library.rs:1802`, with `overwrite = false`.

Upstream:
- MSM: `RunSegmentPluginProviders` `:54-166`, `CreateSegmentAsync` `:169-181`, `DeleteSegmentAsync` `:184-191`, `DeleteSegmentsAsync` `:194-213`;
- the only caller: `Emby.Server.Implementations/ScheduledTasks/Tasks/MediaSegmentExtractionTask.cs:96`, with `forceOverwrite = false`;
- no built-in provider: segments come from plugins.

**The `forceOverwrite` question, verified.** Upstream deletes all of the item's segments, whatever their provider, when `forceOverwrite` is set (MSM:76-80). The read the BaseItems ledger passed on compared that with `media_segment_repository.rs`, but that file is the per-provider replace of the normal path. Ferrofin's overwrite branch is in the manager. It also deletes every row of the item before the providers run (msm:847-864, through `clear_stored` at msm:350-357). Without overwrite, both delete and rewrite one provider's rows only when its answer changed: MSM:99, :113-129 against msm:366-417 and `media_segment_repository.rs:41-56`. Neither side passes `true` anywhere in production. Match.

| Column | Ferrofin writer | Upstream writer | Verdict | Note |
|---|---|---|---|---|
| Id | `Uuid::new_v4()` per published segment (msm:396-403); `create_stored` keeps a non-nil id (msm:330-334) | the DTO's id, or a new one from EF when unset (`ValueGeneratedOnAdd`, `src/Jellyfin.Database/Jellyfin.Database.Providers.Sqlite/Migrations/JellyfinDbModelSnapshot.cs:842-844`) | match | random v4 on both. Ferrofin replaces a provider-set id, but no Ferrofin provider can set one: the WASM `media-segment` record has no id field (`crates/ferrofin-wasm/wit/ferrofin-plugin.wit:370-377`) |
| EndTicks | `media_segment_repository.rs:57-70` | MSM:270-281 | match | an end before the start is refused on both (`media_segment_repository.rs:33-40`; MSM:171) |
| ItemId | as above | MSM:147 | match | |
| SegmentProviderId | `media_segment_provider_id` (`crates/ferrofin-traits/src/media_segments.rs:47-51`) | `GetProviderId` (MSM:308-311) | match | MD5 of the lowercased name, in the `N` format. The Intro Skipper extension's name `Intro Skipper` comes from the plugin's `Plugin.ProviderName` (`crates/ferrofin-traits/src/intro_skipper.rs:23-25`). That was not checked against the plugin's source, which is not in the upstream checkout |
| StartTicks | as EndTicks | as EndTicks | match | |
| Type | `json_value()`, the C# enum value | `(int)MediaSegmentType` | match | |

**Deletion.**
- Item delete: Ferrofin deletes the rows explicitly (ips:2346-2352); no foreign key reaches this table. Upstream does the same (IPS:151). Match.
- External-data delete: `crates/ferrofin-core/src/external_data_manager.rs:164-178` against `Emby.Server.Implementations/Library/ExternalDataManager.cs:52-61`. Match.
- When a video's file changes, upstream prunes its extracted data (`MediaBrowser.Providers/Manager/MetadataService.cs:391-395`); Ferrofin does not: fix (gl-1440). That covers `KeyframeData` and `TrickplayInfos` too.

## TrickplayInfos

Ferrofin (tm):
- `create_tiles_in` `:701-761`;
- `import_tiles` `:771-802`;
- `discover_existing_trickplay` `:852-935`;
- `save_trickplay_info` `:1571-1598`, an upsert on `(ItemId, Width)`;
- `delete_trickplay_data` `:1600-1607` and `delete_trickplay_row` `:350-357`;
- `tile_bitrate` `:1836-1846`.

Upstream (TM):
- `DiscoverExistingTrickplayAsync` `:140-275`;
- the import branch `:437-475`;
- `CreateTiles` `:559-628`;
- `SaveTrickplayInfo` `:713-728`, remove then add;
- `DeleteTrickplayDataAsync` `:731-738`.

The trickplay divergences are recorded on the `trickplay` row of `VERIFIED_SUBSYSTEMS` (`crates/ferrofin-api/src/handlers.rs:1541-1640`).

| Column | Ferrofin writer | Upstream writer | Verdict | Note |
|---|---|---|---|---|
| ItemId | tm:1571-1598 | TM:713-728 | match | 16/16 |
| Width | `create_tiles_in` tm:709-711; import tm:779-782 | TM:573; import TM:454 | match; recorded edge | 16/16. When it imports a folder that has no row, Ferrofin stores the width the folder was made at. Upstream stores the selected width, which names a folder that does not exist: "Jellyfin bug not ported (gl-1246)" (`handlers.rs:1599-1603`) |
| Height | the first tile's height (tm:737-749); import tm:791 | TM:604-608; import TM:466 | match | 16/16 |
| TileWidth | the grid at that width (`options_at_width`) | the configured grid | match; recorded edge (gl-1400) | 16/16 at width 320. A set wider than 320 gets a smaller grid (owner decision 2026-11-05, `handlers.rs:1627-1637`). By that record, an adopted Jellyfin set wider than 320 at the configured grid is regenerated at the smaller grid; not run here |
| TileHeight | as TileWidth | as TileWidth | match; recorded edge (gl-1400) | as above |
| ThumbnailCount | `images.len()` (tm:715); import: the number of `.jpg` tiles (tm:783); discovery `discovered_timing` (tm:1776-1811) | TM:577; import `existingFiles.Length` (TM:458); discovery TM:207-243 | match; recorded edge | 16/16. The import counts only `.jpg` tiles where upstream counts every file: "Jellyfin bug not ported" (`handlers.rs:1596-1598`). Both sides store a tile count there, not a thumbnail count. Discovery uses the same banker's rounding |
| Interval | the configured interval; discovery `discovered_timing` | TM:574; discovery TM:219-243 | match | 16/16 |
| Bandwidth | `tile_bitrate` in f64 (tm:1836-1846), used by all three paths | `CreateTiles` and discovery in decimal (TM:611, :265); the import branch divides by `Interval / 1000` in integers (TM:467) | owner? (gl-1596); owner? edge (P7) | J 289 and F 285 on 15 rows; Bright Lattice J 299, F 288, a difference of 11, not 4. Same formula, different tile bytes, measured on the runs' tile files: each item has one tile, `320 - 10x10/0.jpg`. On the 15 items it is 36103 bytes in J and 35621 in F, and on Bright Lattice 37313 and 35936. With `TileWidth` and `TileHeight` 10 and `Interval` 10000 ms on every row, the formula is ceil(bytes × 8 / 1000): 289 and 285, 299 and 288, the stored values. Owner decision open on gl-1596. A kept J's 289, so the difference arises only for sets Ferrofin generates. FullChroma (owner decision D6, gl-1128, off by default) changes the tile bytes when it is on: recorded (`handlers.rs:1604-1607`). The import edge is P7 |

**Deletion.** Item delete: Ferrofin deletes explicitly (ips:2351), as upstream does (IPS:155); no foreign key reaches the table. Match. An orphaned row is pruned the same way on both sides (tm:807-843 against TM:144-174). On a changed file: gl-1440.

## Adoption (A against J)

- **`Peoples` and `PeopleBaseItemMap`:** no row changed. Both tables are identical to J, rowid for rowid. No adoption repair touched them on this fixture, and the rescan took the unchanged path for every one of the 24 credited items.
  - **Indexes.** Ferrofin never overrides SQLite's built-in `lower()`: its casing functions have names of their own (`crates/ferrofin-db/src/sqlite_casing.rs:1-5`). So the keys its inserts write into `IX_Peoples_NameLower` are the ones Jellyfin's engine computes.
  - **Outside this ledger.** A also carries Ferrofin's own `FerrofinIX_Peoples_NameNoCase_Cover` and `FerrofinIX_PeopleBaseItemMap_PeopleId` (migration `0022`). That is schema, not a writer: the export's concern.
- **`ItemValues`, `ItemValuesMap`, `LinkedChildren`, `AncestorIds`:** A holds J's rows: the 7 values with J's ids, the 38 links, the 11 linked children and the 156 ancestor rows.
- **`BaseItemImageInfos`:** 42 of J's 44 rows kept, ids and EF text included.
  - The two album rows were rewritten: path, id, blurhash and date (gl-1564, fix gl-1633).
  - Six track rows were added (gl-692).
  - The root, the library tiles, the collages and the upload kept J's `{xx}/{idN}/poster.*` paths and were not redrawn. Ferrofin counts Jellyfin's folder as the item's own (di:979-986), so its date gate keeps them.
- **`KeyframeData`** (15/15) and **`TrickplayInfos`** (16/16): kept unchanged.
- **`MediaSegments`:** empty here. The gl-1558 ledger measured it kept on both its adoption routes: a Jellyfin 12.4 database upgraded from 12.3, and a 10.11.8 one.

## SQL that upstream never runs

On these tables, beyond what the rows above cover:
- **Fresh databases only**, baselined on adoption (`crates/ferrofin-db/src/database.rs:1523-1559`): migration `0007` (`crates/ferrofin-db/migrations/0007_jellyfin_10118_parity.sql`):
  - uppercases the GUIDs of nine of the ten tables, all but `LinkedChildren`: `AncestorIds` (`:341-342`), `BaseItemImageInfos` (`:344-345`), `ItemValues` and `ItemValuesMap` (`:350-352`), `KeyframeData` (`:353`), `MediaSegments` (`:354-355`), `PeopleBaseItemMap` and `Peoples` (`:357-359`) and `TrickplayInfos` (`:360`);
  - rewrites a `BaseItemImageInfos.DateModified` written as ISO text with a `T` into `strftime('%Y-%m-%d %H:%M:%f')` text, three fractional digits (`:420`).
- **Shapes earlier Ferrofin builds left:**
  - `0031` rewrites a `CleanValue` only where it holds Ferrofin's previous derivation (`0031_invariant_clean_names.sql:15-18`);
  - `0034` folds `FerrofinLinkedChildren` into `LinkedChildren`;
  - the library-structure merges (`library_structure_repository.rs`) and `rekey_container`.
- **Owner decisions:**
  - the scan's rekeys and folds (`crates/ferrofin-traits/src/persistence.rs:1563-1566`, D4 and D9b): recorded. They carry `MediaSegments` and `TrickplayInfos` to the new id, drop rows stale under it, and move image paths in a folder named after the item (ips:4566-4586 and :5042-5049);
  - `0044` (the `AncestorIds` table above).
- **Ports of upstream migrations:** `0042` ports 12.1's orphaned-parent block of the EF migration `ChangeOwnerIdToGuid`, with the same end state. The ports of the migration routines are under "Upstream migration routines".
- **The person collapse** moves the duplicate's image rows to the kept person (ppl:476-495). Its `UPDATE OR IGNORE` never ignores, because the key is `Id`, so the kept person can end up with two rows of one type. Code read only.
- **The library merge** moves image rows of types the target lacks (`library_structure_repository.rs:358-367`).
- **The segment-producer seams** for the Intro Skipper extension and WASM plugins (`delete_provider_segments`, `delete_all_provider_segments`, `clear_all_provider`: msm:295-323, :537-557) have no upstream core equivalent.
- **Migration `0037_intro_skipper_segment_store.sql:51-53`** renames Ferrofin's legacy provider id `IntroSkipper` to the MD5 form. A Jellyfin database holds no such row, so on adoption it changes nothing.

## Upstream migration routines

All 44 routines under `Jellyfin.Server/Migrations/Routines/` at `v12.4` were searched for the ten tables' names and for item saves and deletes. These are the ones that write one of the ten tables, with what Ferrofin does for the same databases. The EF schema migrations (`src/Jellyfin.Database/Jellyfin.Database.Providers.Sqlite/Migrations/`) are not in this inventory, apart from the one `0042` ports (above).

**Already run on every database Ferrofin adopts:** not needed. Each id is in the 10.11.8 history (`JELLYFIN_10_11_8_MIGRATIONS`, `crates/ferrofin-db/src/database.rs:1232-1301`), which every generation Ferrofin adopts holds (`JELLYFIN_GENERATIONS`, `:1523-1559`). A database Ferrofin created holds none of the older data they convert.

| Routine | What it writes |
|---|---|
| `20250420200000_MigrateLibraryDb` | copies 10.10's `library.db` into `Peoples`, `PeopleBaseItemMap`, `ItemValues`, `ItemValuesMap`, `AncestorIds` and `BaseItemImageInfos` |
| `20250420150000_FixPlaylistOwner` | re-saves playlists through `UpdateToRepositoryAsync` (`:58`, `:65`), the save that writes their values, ancestors, links and images |
| `20250420180000_FixAudioData` | re-saves audio items through `SaveItems` (`:70`), the same save |
| `20250421000000_MigrateKeyframeData` | refills `KeyframeData` from the keyframe cache files (`:62-85`) |
| `20250620180000_FixDates` | rewrites `BaseItemImageInfos.DateModified` (`FixBaseItemImageInfos`, `:122-149`) |
| `20251009200000_CleanMusicArtist` | deletes the `Artist` and `AlbumArtist` rows of `Peoples` (`:39-44`) |

**Run on a 10.11 database's first 12.x boot.** A 10.11 database Ferrofin adopts has not run these, so Ferrofin's one-shot boot repairs do their work (`run_all`, `crates/ferrofin-core/src/adoption_repairs.rs:63`). A 12.x database already ran them in Jellyfin, and the repairs leave it as it is.

| Routine | What it writes | Ferrofin, for the same databases |
|---|---|---|
| `20260113120000_MigrateLinkedChildren` | `LinkedChildren`, from the `Data` JSON | ported: `import_membership_once` (`adoption_repairs.rs:211`) → `import_membership` (`crates/ferrofin-core/src/item_data.rs:1130-1188`). Its cleanup sweeps are gl-521 |
| `20260113230000_CleanupOrphanedExtras` | deletes the extras owned by the placeholder through `DeleteItemsUnsafeFast` (`:102`), so through upstream's item deletion (IPS:134-156) | ported: `cleanup_orphaned_extras` (`adoption_repairs.rs:384-404`), through `delete_items`. The item-deletion rows above apply, `Peoples` included (gl-439) |
| `20260115120000_FixIncorrectOwnerIdRelationships` | deletes items sharing a `Path` (`:134-166`), the same deletion; reads `LinkedChildren` to set `PrimaryVersionId` (`:311-357`) | ported: `fix_owner_id_relationships` (`adoption_repairs.rs:420-487`). Its duplicate deletes go through `fold_into` and were not compared table by table |
| `20260508120000_MergeDuplicateMusicArtists` | re-points `AncestorIds` and both ends of `LinkedChildren` from case-only duplicate artists to a keeper (`:114-152`), then deletes the duplicates (`:201-208`) | ported: `merge_duplicate_music_artists` (`adoption_repairs.rs:3136`), `repoint_references` (`:3280-3315`), re-pointing with the same statements but one: a moved `LinkedChildren` parent row's `SortOrder` is shifted past the keeper's (`:3293-3296`), where upstream's update keeps it and can hit the `(ParentId, SortOrder)` key. That needs a duplicate artist that is itself a `LinkedChildren` parent, which none of the writers in the LinkedChildren section makes. The duplicates are then deleted by a plain `DELETE` and its cascades (`:3266-3270`), not through the item deletion; the two deletes were not compared table by table |
| `20260508130000_MergeDuplicatePeople` | the same fold for `Person` items (`:121-210`), then `Peoples` and `PeopleBaseItemMap` (`:218-310`) | ported: `merge_duplicate_people` (`adoption_repairs.rs:3155`), through `merge_case_duplicates` (the same re-point and plain delete) and `merge_peoples_rows` (the case-only duplicate row of the credit tables) |
| `20260610120000_RefreshCleanNamesAndValues` | `ItemValues.CleanValue` (`:117-163`) | ported: `refresh_clean_values` (ips:1177-1222) |
| `20260729120000_RestorePlaylistChildrenFromMetadata` | `LinkedChildren` for a playlist the earlier steps left with none, whose `Path` is a directory holding a `playlist.xml`: one `Manual` row per `<PlaylistItem><Path>` entry that names an item's exact path (`:38-145`) | gap: fix (verify first), F10 (gl-1648). An adopted 10.11 playlist gets its members only from `Data` (`import_membership_once`), and nothing reads the `playlist.xml` of a playlist in the data directory: the scan reads it only for playlist folders in a media library, and never walks the data directory's `playlists` folder (`crates/ferrofin-core/src/library_scan/playlists.rs:45-49`). Reachability is plausible and narrow: the routine mainly repairs 12.0 release-candidate databases, which Ferrofin cannot adopt, and on a direct 10.11 upgrade it acts only where `Data` is unparseable or a `playlist.xml` is staler than `Data`. In the stale case it brings back entries a user removed (gl-1648 sets out both) |
| `20260825200000_ConsolidateLocalizedUserViews` | `AncestorIds` (`MoveAncestorsAsync`, `:317-361`); deletes the stale views' `LinkedChildren` rows (`:189-197`) and the views themselves (`:161-165`) | ported: `consolidate_localized_user_views` (`crates/ferrofin-core/src/user_view_repository.rs:95`): `move_ancestors` (`:433-481`), `move_remaining_references` (`:316-337`) and the delete (`:294`) |

Two routines that touch `LinkedChildren` write none of the ten tables:
- `20260908120000_RepairAlternateVersionLinks` (12.1) reads `LinkedChildren` and writes only `BaseItems`: `PrimaryVersionId` and `PresentationUniqueKey` (`:46-137`). Ported: `repair_alternate_version_links` (`adoption_repairs.rs:811`). Its `BaseItems` columns are gl-1558's ledger.
- `20260911120000_StripEmbeddedLinkedChildren` (12.1) rewrites only `BaseItems.Data` (`:31-33`). Ported: `strip_embedded_linked_children` (`adoption_repairs.rs:999`).

`20260525010000_CleanupOrphanedExternalData` writes no table either. It deletes only folders on disk (attachments, subtitles, trickplay tiles and chapter images) of items no longer stored, and its doc comment leaves the database side to `DeleteItem` (`:17-21`). The other routines name none of the ten tables, and none writes them through an item save or delete.

## Not covered

- **Read in code only.** No fixture writes:
  - a person with a non-ASCII capital or two casings of one name, so P1, P2 and the case-only role rule are read from the code only;
  - remote provider credits (TMDB, OMDb and TVDB roles, `SortOrder` from billing): the runs block providers;
  - Studios and Tags values;
  - Live TV values, ancestors and artwork;
  - merges, splits and promotions of versions, and box-set removals;
  - remote image rows (provider images before conversion), person images and chapter images, so gl-1639 is from code alone;
  - `MediaSegments` values. The Intro Skipper extension's publish step (seconds to ticks, its type mapping) was not compared with the third-party plugin's `SegmentProvider`, which is not in the checkout.
- **Not compared case by case:**
  - `promote_version`'s row order (`ROW_NUMBER() OVER (ORDER BY "ChildType", "SortOrder")`, ips:3604-3620) against upstream's save of the new primary;
  - a merge whose primary was itself a version. Ferrofin clears its pointer and link (`library_manager.rs:3409-3414`); upstream's `MergeVersions` (`Jellyfin.Api/Controllers/VideosController.cs:187-251`) does not touch the primary's own `PrimaryVersionId`;
  - `drop_version` also strips the deleted path from the primary's `LocalAlternateVersions` in `Data`, which upstream (LM:545-550) does not appear to do. That is the BaseItems ledger's `Data` column (gl-1563); it was not looked into.
- **The adoption repairs' duplicate deletes.** `fix_owner_id_relationships`, `merge_duplicate_music_artists` and `merge_duplicate_people` delete their duplicates through `fold_into` or a plain `DELETE` and its cascades; those deletes were not compared table by table with upstream's routines (the inventory rows above).
- **A missing ancestor.** The `AncestorIds` row "an ancestor with no row" traces every production caller of `set_ancestors` from the code. The one route found, an ancestor deleted by a concurrent `DELETE /Items/{id}` during a scan, was not run, and whether a delete and a scan can interleave there was not traced further.
- **TMDB credits and `AddPerson`.** Ferrofin's TMDB rows skip `AddPerson`'s rules (Input side). For example, a TMDB episode answer naming one person in both `cast` and `guest_stars`, with the same character, would give upstream one `GuestStar` credit (`MediaBrowser.Controller/Entities/PeopleHelper.cs:81-85`, :65-69) and Ferrofin two, an `Actor` and a `GuestStar` (`crates/ferrofin-providers/src/tmdb.rs:633-634`). Whether TMDB answers ever do that was not checked; gl-1651 (F11) tracks the fix.
- **The migration inventory** covers the routines under `Jellyfin.Server/Migrations/Routines/` only. Data steps inside the EF schema migrations were not listed, apart from the orphaned-parent block `0042` ports.
- **Trickplay** at widths other than 320, and the import and discovery branches, were not exercised. The tile sizes were measured (the `Bandwidth` row), but why the two servers' tiles differ in size was not looked into (gl-1596).
- **The upload route** was measured only after the scan that followed it, since uploads are stored without size or hash (gl-1054). The swap route was not run.
- **The messy and scale adoption fixtures.** They hold duplicate and orphan people. Not run here; the adoption gate checks them against Jellyfin's own results.
- **The return.** Jellyfin was not booted on F or A for these tables. For the credit tables every value it would read is equal, and the extra indexes are noted under Adoption.

## Follow-ups

Filed or extended by this audit. Each is linked to gl-1560.

**Release status.** The follow-ups are filed outside the milestone with no `target::` label, per the project rule for findings in a targeted milestone: they stay outside it, linked, until the owner says which of them block the release. The owner is asked on gl-1560 which, if any, should block v1.6.0. None of them blocks another.

**Searches.** Searched first, open and closed: GitLab for `SupportsPeople`, `supports_people`, `update_people`, `item_values_of`, `ItemValues`, `ItemValuesMap`, `save_item_values`, `LinkedChildren`, `SortOrder`, `remove_from_collection`, `drop_version`, `AncestorIds`, `channel AncestorIds`, `1970`, `epoch`, `MinValue`, `ItemImageInfo`, `DateModified`, `remote image` and related title words, for F11 `AddPerson`, `add_person`, `GuestStar`, `people_entities`, `episode_people_from` and `TMDB credits` (gl-1125 is the TVDB sibling, gl-432 the exact-duplicate dedupe already merged); and for F10 `RestorePlaylistChildrenFromMetadata`, `RestorePlaylist`, `playlist.xml`, `MigrateLinkedChildren` and `import_membership`; the public GitHub issues; and the maintainers' out-of-scope record, which has no entry for these writers. gl-1588 covers the editor's own trim, not `item_values_of`'s; gl-211 covers only the delete half of P5; gl-1591 covers Live TV's `BaseItems` and provider rows, not its ancestors or images; gl-1590 (PR 922) covers chapters' `ImageDateModified` and the chapter-image route, not image rows.

| # | Issue | What |
|---|---|---|
| F1 | gl-1634 | The scan and the provider refresh write credits without upstream's `SupportsPeople` check |
| F2 | gl-1635 | `ItemValues.Value` trimmed by `item_values_of`, where Jellyfin keeps the text and drops only blank entries (confirmed by gl-1588's live check) |
| F3 | gl-1636 | A scan save that leaves an item with no artists, genres, studios or tags keeps its old `ItemValuesMap` rows (reached by "Replace all metadata" when nothing gives back a value) |
| F4 | gl-1637 | Removing items from a box set, or deleting a merged version, leaves gaps in `LinkedChildren.SortOrder` (a box set's for good, a version's until the next scan) |
| F5 | gl-1638 | Live TV channels get no `AncestorIds` row |
| F6 | gl-950 (context added) | library validation draws the root folders' images at the end of the scan, behind their date gate, where upstream refreshes the `AggregateFolder` and the user root first (`ValidateTopLibraryFolders`); measured root `Height` 150 in J, 100 in F |
| F7 | gl-1639 | Remote image rows (Live TV programme artwork) store `DateModified` 1970-01-01 (chrono's default) where Jellyfin stores 0001-01-01 |
| F8 | gl-1054 (context added) | re-measure an image's size and blur hash on every image save, the swap route included, and store upstream's failure values |
| F9 | gl-1575 (context added) | the library-tile writer (`library_scan.rs:14185-14186`) and the photo album's `folder.jpg` naming |
| F10 | gl-1648 | No port of 12.0's `RestorePlaylistChildrenFromMetadata`: an adopted 10.11 playlist that the `Data` import leaves empty is not refilled from its `playlist.xml` |
| F11 | gl-1651 | TMDB credits skip `AddPerson`'s merge rules: a person in both cast and guest stars with one character gets two credits where Jellyfin keeps one |

Already open and cited: gl-464 and gl-439 (people rows left after a dropped credit or a deleted item, both still present), gl-1440 (extracted data kept for a changed file), gl-1618 (datetime text, from gl-1565), gl-1626 (from gl-1561), gl-1564 and its fix gl-1633 (album art), gl-692 (track art), gl-1575, gl-1595, gl-1596 and gl-1597, gl-1624, gl-211, gl-1125 (provider credits), gl-521 (`MigrateLinkedChildren`'s sweeps), and gl-1598 (the seed patch). Owner-decided and cited: D4 and D9b (the rekey fold), D6 (FullChroma), the 2026-11-05 trickplay grid (gl-1400), and `0044` (the adoption-realism milestone).

**For the owner (proposed accepted divergences):**
- **P1: the people row a non-ASCII name resolves to.**
  - **Upstream:** looks a person up with SQLite's ASCII-only `lower(Name)` against .NET's `ToLowerInvariant` (PPL:171-186). A name with an uppercase non-ASCII letter (Icelandic, Spanish, Slavic, Greek) never matches its stored row. So each write that changes a credit list naming that person inserts another `Peoples` row with the same name. A write whose credits are unchanged returns before the lookup (`mappingsAreUnchanged`, PPL:140-161, `return` at :158) and adds nothing.
    - A changed write of an existing item, whose kept credit stays mapped to the old row, leaves the new row unmapped. `DeleteCreditsWithoutMapping` only sweeps the rows of dropped credits, so it survives until Refresh People.
    - A newly credited item maps to the new row, which Refresh People's exact-name dedupe merges weekly.
  - **Ferrofin:** reuses the row (`COLLATE NOCASE`, ppl:1003-1010, 1096-1121).
  - **Effect:** only the number of `Peoples` rows, never what a client sees. Both sides then match a name differing in non-ASCII case alike, and mint a new row.
  - **Evidence:** the two lookups above, and SQLite's built-in `lower()`: `lower('Émile ZOLA')` gives `Émile zola`, `lower('Иван')` gives `Иван`. Microsoft.Data.Sqlite's bundled engine has no ICU, and Jellyfin's tree registers no function. Not run on Jellyfin.
  - **Proposal:** record it as a Jellyfin bug not ported. Upstream's own `MergeDuplicatePeople` migration treats duplicate people rows as a bug.
- **P2: credits rewritten versus updated in place.**
  - **Upstream:** updates an item's kept credits in place and adds or removes only the difference (PPL:203-240).
  - **Ferrofin:** deletes them all and re-inserts them (ppl:1955, 1994). It also rewrites identical credits when a credited person has no `Person` item yet.
  - **Effect:** the values are equal, but for two edges, both possible only where duplicate rows already exist (P1's on an adopted database, or a case-only duplicate):
    - a kept credit can move to the lowest-`Id` duplicate `Peoples` row (and then gl-464's fix deletes the row it left);
    - when two stored roles of one person differ only in case, the casing kept can come from the other row: Ferrofin's `HashMap` keeps the last stored row (ppl:1944-1947), upstream's `TryAdd` the first (PPL:203-207).

    Rowids differ, which nothing reads.
  - **Evidence:** the edited movie's credits are rows 28 and 29 in J and 48 and 49 in F, with equal values.
  - **Proposal:** accept. The alternative is porting the in-place update.
- **P3: a nameless person from the metadata editor.**
  - **Upstream:** fails the request on a null name (PPL:116), and would store an empty or blank one as a credit named `''` (read from the code, not run).
  - **Ferrofin:** drops the entry and saves the rest of the edit (`item_update.rs:318-325`). The comment there covers only the null case.
  - **Proposal:** accept, and extend the comment to the empty and blank cases.
- **P4: Refresh People's duplicate merge on a colliding credit.**
  - **Upstream:** where two same-name `Peoples` rows credit one item with the same role, `ExecuteUpdate` hits the primary key and the task throws (read from the primary key, not run).
  - **Ferrofin:** re-points with `UPDATE OR IGNORE` and drops the colliding credit (`scheduled_tasks/library.rs:911-923`), as `MergeDuplicatePeople` itself does.
  - **Reachability:** none through `UpdatePeople`, which maps one row per name and type per item. Only legacy data the migration has already merged can hold such a pair.
  - **Proposal:** accept, as documented.
- **P5: `InheritedTags` (type 6) values and links.**
  - **Upstream:** on every save, it writes a type-6 value and link for each tag of the item, of its parents and of its libraries (`MediaBrowser.Controller/Entities/BaseItem.cs:1966-1982`; IPS:328, 810). At the end of every library scan's post-scan tasks it deletes every type-6 link (LM:1597 → IPS:161-170). The type-6 `ItemValues` rows stay until the next deletion's prune (IPS:149).
  - **Upstream reads none of them:** 12.4's tag filters read `Tags` (4) with the ancestor closure (`Jellyfin.Server.Implementations/Item/BaseItemRepository.TranslateQuery.cs:1129-1160`, `Jellyfin.Server.Implementations/Item/BaseItemRepository.QueryBuilding.cs:608-640`). A search of v12.4 finds no other use of `ItemValueType.InheritedTags`.
  - **Ferrofin:** it writes no type 6 (`base_items.rs:548-571`), and `update_inherited_values` is a no-op that nothing calls (ips:4395-4401, gl-211).
  - **Effect:** after a scan, Jellyfin holds type-6 `ItemValues` rows with no links where Ferrofin holds none. Between an edit and the next scan, Jellyfin also holds the edited item's type-6 links. No client sees either. Not measured: the fixtures have no tags.
  - **Proposal:** accept the absence, as upstream churn that nothing reads, and close gl-211's half by removing `update_inherited_values`. If the owner declines, D8's default applies: port both halves (the save's type-6 values, and the delete after the post-scan tasks) through gl-211.
- **P6: image row ids kept across saves.**
  - **Upstream:** draws a new id for every image row on every save (Map:455; IPS:183-197, :481-485) and lists an item's images by `Id`, so a list of backdrops reshuffles on each save.
  - **Ferrofin:** keeps the row and id of an unchanged image and lists by insertion order. The comment at ips:6918-6923 explains it, and the reorder fix gl-65 relies on it.
  - **Effect:** none on values. Ids are random on both sides, and Jellyfin redraws them on its first save after a return.
  - **Proposal:** accepted, as documented.
- **P7: the trickplay import branch's bandwidth.**
  - **Upstream:** computes an imported set's `Bandwidth` with `Interval / 1000` in integer division (TM:467). For an interval that is not a whole number of seconds that overstates it (1500 ms divides by 1). Under 1000 ms it throws, and the import fails. Its own `CreateTiles` and discovery use decimal seconds (TM:611, :265), as Ferrofin's `import_tiles` does (tm:795, through `tile_bitrate`).
  - **Effect:** only a folder of tiles without a row, at such an interval. Whether Ferrofin's settings allow one was not checked (gl-1171 set the ranges).
  - **Proposal:** record it as a Jellyfin bug not ported, beside the two import rows at `handlers.rs:1596-1603`.
- **P8: Live TV channel logos kept at their URL.**
  - **Upstream:** sets the channel's Primary to the tuner's or guide's logo URL, and the channel's save (`src/Jellyfin.LiveTv/Guide/GuideManager.cs:316`) runs `UpdateImagesAsync`, which downloads it into a local file (`ConvertImageToLocal`, LM:2584), whose path then never equals the URL, so every refresh re-sets and re-downloads every logo. The stored row holds the file's path and time, or the image is removed when the download fails.
  - **Ferrofin:** keeps the row at the URL and serves it from the remote-image cache, so an unchanged logo is left alone. The comment at `crates/ferrofin-livetv/src/projection.rs:437-447` explains it; a non-URL logo path is recorded as a Jellyfin bug not ported (`projection.rs:406-408`).
  - **Effect:** a channel's Primary row differs in `Path` (the URL against a downloaded file) and `DateModified` (gl-1639's default against the file's time). Clients see the same logo. After a return, Jellyfin's next guide refresh downloads it, as it does on every refresh. Not measured: no fixture configures a tuner.
  - **Proposal:** accept, as documented.

Owner decisions already open on these tables: gl-1596 (`Bandwidth`), gl-692 (the track Primary rows), gl-1595 (the extras copy) and gl-1598 (the first collection's library scan).
