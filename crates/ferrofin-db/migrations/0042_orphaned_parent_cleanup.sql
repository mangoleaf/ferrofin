-- 0042: remove orphaned parent chains — items whose "ParentId" names an item that no
-- longer exists, and everything under them. Port of the orphan block Jellyfin 12.1 added
-- to 20260113203012_ChangeOwnerIdToGuid (src/Jellyfin.Database/Jellyfin.Database.Providers.Sqlite/
-- Migrations/20260113203012_ChangeOwnerIdToGuid.cs:14-80, in its v12.2 form with the
-- "DoomedUserDataKeys" table). 0032 ports only that migration's 12.0 statements, so a
-- 10.11 database or a 12.0-origin one can still carry a dangling "ParentId" (10.11's
-- 20250913211637_AddProperParentChildRelationBaseItemWithCascade deleted only four
-- levels of a dangling chain before adding the foreign key), and the boot's
-- PRAGMA foreign_key_check would then refuse to open it. Never baselined; a no-op on a
-- clean database.
--
-- Ferrofin differences, all because Ferrofin migrations run with foreign_keys = OFF
-- (see database.rs), where upstream's run with them ON:
--  * Upstream's single DELETE FROM "BaseItems" cascades into every child table. Here
--    nothing cascades, so every table with a foreign key to "BaseItems" is cleared by
--    hand below (orphaned_parents.rs checks that list against the live schema), plus
--    the intro-skipper tables, which key rows by item id without a foreign key and are
--    otherwise removed only by the Intro Skipper's CleanCache task (the same end
--    state, reached later).
--    "FerrofinUserDataRetentionSnapshots"."ItemId" is left alone: it names deleted
--    items on purpose.
--  * Items OWNED by an orphan join the orphan set. At upstream's point in its chain
--    "OwnerId" has no foreign key yet; 20260113233000_AddForeignKeyToOwnerId then
--    re-points the dangling owners at the placeholder item and CleanupOrphanedExtras
--    deletes those extras. Here the OwnerId foreign key already exists, so a dangling
--    owner would fail the boot check: the extras go now, with the same end state.
--  * "RetentionDate" is written in the format Ferrofin stores it everywhere else
--    (ferrofin_db::store::datetime_to_db: 'YYYY-MM-DD HH:MM:SS.fffffff'; EF writes the
--    same with trailing zeros trimmed), not datetime('now')'s 'YYYY-MM-DD HH:MM:SS'.
--    The 90-day purge compares it as text against that format.
--
-- Retention limit: delete_items also writes a source snapshot
-- ("FerrofinUserDataRetentionSnapshots" + "…Keys", 0035) whose identity keys are derived
-- in Rust; SQL cannot produce them, and this migration writes none. The play state it
-- moves onto the placeholder is therefore a "legacy" placeholder row, exactly like every
-- detached row in a Jellyfin database: recovery reattaches it only on an exact GUID key
-- match — the same file rescanned at the same path, not a moved one.
--
-- A "ParentId" cycle cannot loop the recursion: UNION (not UNION ALL) queues a row only
-- once, so the walk is bounded by the number of items. A cycle with no dangling parent
-- has no starting row and is left alone, as upstream leaves it (it violates no foreign key).

PRAGMA defer_foreign_keys = ON;

DROP TABLE IF EXISTS "OrphanedBaseItemIds";
CREATE TEMPORARY TABLE "OrphanedBaseItemIds" ("Id" TEXT NOT NULL PRIMARY KEY);

INSERT INTO "OrphanedBaseItemIds" ("Id")
WITH RECURSIVE Orphan ("Id") AS (
    SELECT Child."Id"
    FROM "BaseItems" AS Child
    WHERE Child."ParentId" IS NOT NULL
      AND NOT EXISTS (SELECT 1 FROM "BaseItems" AS Parent WHERE Parent."Id" = Child."ParentId")
    UNION
    SELECT Descendant."Id"
    FROM "BaseItems" AS Descendant
    INNER JOIN Orphan ON Descendant."ParentId" = Orphan."Id"
    UNION
    -- Ferrofin addition: extras owned by an orphan (see the header).
    SELECT Owned."Id"
    FROM "BaseItems" AS Owned
    INNER JOIN Orphan ON Owned."OwnerId" = Orphan."Id"
)
SELECT "Id" FROM Orphan;

-- Keep the play state of the doomed items the way ItemPersistenceService does when it
-- deletes an item: reattach it to the placeholder item instead of letting the
-- FK_UserData_BaseItems_ItemId cascade wipe it. The placeholder can only hold one row
-- per (UserId, CustomDataKey), so resolve collisions before repointing anything.
DROP TABLE IF EXISTS "DoomedUserDataKeys";
CREATE TEMPORARY TABLE "DoomedUserDataKeys" (
    "UserId" TEXT NOT NULL,
    "CustomDataKey" TEXT NOT NULL,
    PRIMARY KEY ("UserId", "CustomDataKey"));

-- Collect the colliding keys up front: correlating against "UserData" directly makes
-- the delete below re-scan every row of that user once per placeholder row.
INSERT OR IGNORE INTO "DoomedUserDataKeys" ("UserId", "CustomDataKey")
SELECT Doomed."UserId", Doomed."CustomDataKey"
FROM "UserData" AS Doomed
INNER JOIN "OrphanedBaseItemIds" AS Orphan ON Orphan."Id" = Doomed."ItemId";

DELETE FROM "UserData"
WHERE "ItemId" = '00000000-0000-0000-0000-000000000001'
  AND EXISTS (
      SELECT 1
      FROM "DoomedUserDataKeys" AS Doomed
      WHERE Doomed."UserId" = "UserData"."UserId"
        AND Doomed."CustomDataKey" = "UserData"."CustomDataKey");

DELETE FROM "UserData"
WHERE "ItemId" IN (SELECT "Id" FROM "OrphanedBaseItemIds")
  AND "rowid" NOT IN (
      SELECT MIN("rowid")
      FROM "UserData"
      WHERE "ItemId" IN (SELECT "Id" FROM "OrphanedBaseItemIds")
      GROUP BY "UserId", "CustomDataKey");

UPDATE "UserData"
SET "ItemId" = '00000000-0000-0000-0000-000000000001',
    "RetentionDate" = strftime('%Y-%m-%d %H:%M:%f', 'now') || '0000'
WHERE "ItemId" IN (SELECT "Id" FROM "OrphanedBaseItemIds");

-- FK_LinkedChildren_BaseItems_{ParentId,ChildId} are NO ACTION, so these rows have to
-- go by hand or the delete below fails on them.
DELETE FROM "LinkedChildren"
WHERE "ParentId" IN (SELECT "Id" FROM "OrphanedBaseItemIds")
   OR "ChildId" IN (SELECT "Id" FROM "OrphanedBaseItemIds");

-- ── Ferrofin-only: what upstream's cascade would have removed ────────────
-- One statement per foreign key to "BaseItems" (foreign_keys is OFF, so ON DELETE
-- CASCADE does not fire). "BaseItems"."ParentId"/"OwnerId" need none: every child and
-- every owned extra of an orphan is itself in the orphan set.
DELETE FROM "AncestorIds" WHERE "ItemId" IN (SELECT "Id" FROM "OrphanedBaseItemIds");
DELETE FROM "AncestorIds" WHERE "ParentItemId" IN (SELECT "Id" FROM "OrphanedBaseItemIds");
DELETE FROM "AttachmentStreamInfos" WHERE "ItemId" IN (SELECT "Id" FROM "OrphanedBaseItemIds");
DELETE FROM "BaseItemImageInfos" WHERE "ItemId" IN (SELECT "Id" FROM "OrphanedBaseItemIds");
DELETE FROM "BaseItemMetadataFields" WHERE "ItemId" IN (SELECT "Id" FROM "OrphanedBaseItemIds");
DELETE FROM "BaseItemProviders" WHERE "ItemId" IN (SELECT "Id" FROM "OrphanedBaseItemIds");
DELETE FROM "BaseItemTrailerTypes" WHERE "ItemId" IN (SELECT "Id" FROM "OrphanedBaseItemIds");
DELETE FROM "Chapters" WHERE "ItemId" IN (SELECT "Id" FROM "OrphanedBaseItemIds");
DELETE FROM "FerrofinPlaylistShares" WHERE "PlaylistId" IN (SELECT "Id" FROM "OrphanedBaseItemIds");
DELETE FROM "FerrofinPlaylists" WHERE "PlaylistId" IN (SELECT "Id" FROM "OrphanedBaseItemIds");
DELETE FROM "ItemValuesMap" WHERE "ItemId" IN (SELECT "Id" FROM "OrphanedBaseItemIds");
DELETE FROM "KeyframeData" WHERE "ItemId" IN (SELECT "Id" FROM "OrphanedBaseItemIds");
DELETE FROM "MediaStreamInfos" WHERE "ItemId" IN (SELECT "Id" FROM "OrphanedBaseItemIds");
DELETE FROM "PeopleBaseItemMap" WHERE "ItemId" IN (SELECT "Id" FROM "OrphanedBaseItemIds");
-- Intro-skipper rows (no foreign key; see the header). Their ids are normally the
-- store's lowercase spelling, but may hold the uppercase "BaseItems" one
-- (intro_skipper_repository.rs, `spellings`), so match both.
DELETE FROM "FerrofinIntroSkipperSegments" WHERE "ItemId" IN (
    SELECT "Id" FROM "OrphanedBaseItemIds" UNION ALL SELECT lower("Id") FROM "OrphanedBaseItemIds");
DELETE FROM "FerrofinIntroSkipperDisabledEpisodes" WHERE "EpisodeId" IN (
    SELECT "Id" FROM "OrphanedBaseItemIds" UNION ALL SELECT lower("Id") FROM "OrphanedBaseItemIds");
DELETE FROM "FerrofinIntroSkipperDisabledEpisodes" WHERE "SeasonId" IN (
    SELECT "Id" FROM "OrphanedBaseItemIds" UNION ALL SELECT lower("Id") FROM "OrphanedBaseItemIds");
DELETE FROM "FerrofinIntroSkipperSeasonStates" WHERE "SeasonId" IN (
    SELECT "Id" FROM "OrphanedBaseItemIds" UNION ALL SELECT lower("Id") FROM "OrphanedBaseItemIds");

DELETE FROM "BaseItems" WHERE "Id" IN (SELECT "Id" FROM "OrphanedBaseItemIds");

DROP TABLE "DoomedUserDataKeys";
DROP TABLE "OrphanedBaseItemIds";
