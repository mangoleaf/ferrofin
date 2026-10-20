-- 0045: re-point an OwnerId that names no item at the placeholder item.
--
-- "BaseItems"."OwnerId" has a foreign key to "BaseItems"."Id"
-- (FK_BaseItems_BaseItems_OwnerId). A 10.11 source gets this from 0032, which ports
-- 20260113203012_ChangeOwnerIdToGuid and 20260113233000_AddForeignKeyToOwnerId
-- (Migrations/…ChangeOwnerIdToGuid.cs:16-39, …AddForeignKeyToOwnerId.cs:26-32).
-- A 12.x source has the 12.x shape already and never passes through those statements,
-- so an item whose owner is gone (rows written while foreign keys were off, or by an
-- older server) reached the every-boot foreign_key_check (database.rs) and was refused,
-- although Jellyfin 12.2 boots the same file.
--
-- The last statement is 0032's, unchanged: the item is not nulled but handed to the
-- placeholder item (00000000-0000-0000-0000-000000000001, which Jellyfin's own database
-- always carries), and the CleanupOrphanedExtras boot repair (ferrofin-core,
-- adoption_repairs.rs) then deletes exactly those items with their children, the end
-- state a 10.11 source reaches: delete what the key would have cascaded, never refuse
-- the boot. 0044 does the same for rows of missing users and missing ancestry items.
--
-- Two statements before it are not upstream's repair but keep that repair from deleting
-- an item whose owner is merely misspelled, where the boot used to refuse loudly:
--  * SQLite compares ids byte for byte, so an owner that exists but is spelled in
--    another case than its item's Id (Jellyfin reads it as the same Guid and boots on
--    it) would count as dangling. Such an owner keeps its item and takes the spelling
--    of the Id. 0032 does this by upper-casing OwnerId, but only on a 10.11 source.
--  * An OwnerId that is not 36 characters long, or the all-zero Guid, names no item either. 0032 maps
--    them to NULL (an unowned item); so do we, instead of deleting the item.
--
-- A new migration rather than a line in 0044: a database that already applied 0044
-- refuses a changed file (sqlx checksum). Not in REBUILD_MIGRATIONS (database.rs) on
-- purpose: this deletes nothing, and the repair's deletes come after the pre-0044
-- snapshot on any upgrade where 0044 and 0045 arrive together.
--
-- Nothing to log here: on the adoption boot the repair logs how many items it removed.
-- A no-op on a clean database. The CASE keeps the two scans below to the rows that
-- already dangle byte for byte, whatever order the planner picks.

UPDATE "BaseItems" SET "OwnerId" = NULL
 WHERE "OwnerId" IS NOT NULL
   AND (length("OwnerId") != 36 OR "OwnerId" = '00000000-0000-0000-0000-000000000000')
   AND "OwnerId" NOT IN (SELECT "Id" FROM "BaseItems");

UPDATE "BaseItems"
   SET "OwnerId" = (SELECT o."Id" FROM "BaseItems" o
                     WHERE upper(o."Id") = upper("BaseItems"."OwnerId") ORDER BY o."Id" LIMIT 1)
 WHERE CASE WHEN "OwnerId" IS NOT NULL AND "OwnerId" NOT IN (SELECT "Id" FROM "BaseItems")
            THEN EXISTS (SELECT 1 FROM "BaseItems" o WHERE upper(o."Id") = upper("BaseItems"."OwnerId"))
            ELSE 0 END;

UPDATE "BaseItems" SET "OwnerId" = '00000000-0000-0000-0000-000000000001'
 WHERE "OwnerId" IS NOT NULL AND "OwnerId" NOT IN (SELECT "Id" FROM "BaseItems");
