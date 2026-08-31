-- 0028: finish the `PresentationUniqueKey` backfill 0027 started.
--
-- 0027 was scoped `"TopParentId" IS NOT NULL` and wrote the row's own id for
-- EVERY kind. Both halves were wrong, and measured wrong on the parity lab:
--
--   * an item-by-name row (`Genre`/`MusicGenre`/`Person`/`Studio`/
--     `MusicArtist`) has `TopParentId` NULL by construction — it hangs off no
--     library — so 0027 skipped exactly the rows two of the three write paths
--     it names had left keyless. After 0027 applied, the lane's own database
--     still held 3 keyless `Person` rows and 3 keyless `MusicGenre` rows, and
--     because the recursive universe groups on a BARE
--     `GROUP BY "PresentationUniqueKey"` (upstream's own
--     `dbQuery.GroupBy(e => e.PresentationUniqueKey)`,
--     Jellyfin.Server.Implementations/Item/BaseItemRepository.cs:417) and
--     SQLite groups NULLs together, all six shared ONE group:
--     `GET /Items?userId=…&ids=<the three Person ids>` answered with a single
--     person where Jellyfin answered with three.
--   * the own-id VALUE is only correct for the kinds that keep
--     `BaseItem.CreatePresentationUniqueKey()`. The five kinds above override
--     it with `GetType().Name + "-" + Name.RemoveDiacritics()`
--     (MediaBrowser.Controller/Entities/Genre.cs:37-47, and `Artist` for
--     `MusicArtist`, MusicArtist.cs:152) — read back out of the oracle's own
--     database as `Person-Alice Parity`, `MusicGenre-Ambient`.
--
-- So this statement widens the scope (no `TopParentId` predicate) and NARROWS
-- the value's reach: it writes the own-id default only for the kinds whose key
-- really is the own id. `RemoveDiacritics` is a Unicode NFD fold plus a
-- ligature map and cannot be spelled in SQLite, so the five by-name kinds are
-- repaired in Rust instead, by
-- `ferrofin_db::presentation_key::backfill_by_name_presentation_keys`, which
-- calls the SAME helper the inserts call. Guessing their value here is what
-- 0027 did.
--
-- Excluded, deliberately:
--   * `LiveTvProgram` — upstream leaves an airing's key NULL (the guide refresh
--     calls `RefreshMetadata` on the CHANNEL only,
--     src/Jellyfin.LiveTv/Guide/GuideManager.cs:305), and the NULL group
--     collapsing the whole guide into one `Program` row on an unfiltered
--     recursive page is the behaviour, not a defect.
--   * `PLACEHOLDER` — the seed row, which the grouped subquery excludes by id.
--
-- Rows that already carry a key are untouched: a merged version's key is its
-- PRIMARY's id and a season's is derived from its series.
UPDATE "BaseItems"
   SET "PresentationUniqueKey" = lower(replace("Id", '-', ''))
 WHERE ("PresentationUniqueKey" IS NULL OR "PresentationUniqueKey" = '')
   AND "Type" NOT IN (
        'MediaBrowser.Controller.LiveTv.LiveTvProgram',
        'MediaBrowser.Controller.Entities.Genre',
        'MediaBrowser.Controller.Entities.Audio.MusicGenre',
        'MediaBrowser.Controller.Entities.Person',
        'MediaBrowser.Controller.Entities.Studio',
        'MediaBrowser.Controller.Entities.Audio.MusicArtist',
        'PLACEHOLDER');
