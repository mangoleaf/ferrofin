-- Every row inside the recursive user universe carries a
-- `PresentationUniqueKey`, because the universe is grouped on it.
--
-- `BaseItemRepository.ApplyGroupingFilter` runs `GroupBy(e =>
-- e.PresentationUniqueKey)` whenever `EnableGroupByPresentationUniqueKey` holds
-- — which it does for any user query that names no `IncludeItemTypes`
-- (v10.11.8 Jellyfin.Server.Implementations/Item/BaseItemRepository.cs:409-418
-- and :1557-1589). SQLite groups NULLs together, so every keyless row collapses
-- into ONE result: on a real 10.11.8 that is deliberate and applies to exactly
-- one kind, `LiveTvProgram`, whose airings never pass through
-- `MetadataService.UpdatePresentationUniqueKey` (the guide refresh calls
-- `RefreshMetadata` on a CHANNEL only, src/Jellyfin.LiveTv/Guide/
-- GuideManager.cs:305) — which is why an unfiltered recursive page shows one
-- `Program` and not the whole guide.
--
-- Three Ferrofin write paths left OTHER rows keyless: the container insert
-- behind a collection/playlist (`insert_named_item`), the `MusicGenre` by-name
-- row, and the scan-path `Person` row. All three are fixed at the insert, but
-- an existing database still holds the rows they wrote, and a keyless
-- collections library would share the guide's NULL group and disappear from
-- the user's own home query.
--
-- The value is `BaseItem.CreatePresentationUniqueKey()`'s default — the row's
-- own id in the dashless `N` form, lowercased, the spelling
-- `kinds::presentation_unique_key` produces. Rows that already have a key are
-- untouched (a merged version's key is its PRIMARY's id, and a season's is
-- derived from its series), and `LiveTvProgram` is excluded so the guide keeps
-- collapsing the way Jellyfin's does.
UPDATE "BaseItems"
   SET "PresentationUniqueKey" = lower(replace("Id", '-', ''))
 WHERE ("PresentationUniqueKey" IS NULL OR "PresentationUniqueKey" = '')
   AND "TopParentId" IS NOT NULL
   AND "Type" <> 'MediaBrowser.Controller.LiveTv.LiveTvProgram';
