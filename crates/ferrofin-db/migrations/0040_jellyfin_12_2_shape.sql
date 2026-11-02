-- 0040: Jellyfin 12.2's only schema change, 20261002210711_MakeOwnerIdIndexesPartial
-- (src/Jellyfin.Database/Jellyfin.Database.Providers.Sqlite/Migrations/, v12.2): the
-- OwnerId and ExtraType/OwnerId indexes become partial so extras lookups keep using
-- them. DDL copied byte-for-byte from a real 12.2 database (tests/data/jellyfin-12.2-schema.sql).
-- Baselined (recorded, never executed) on an adopted 12.2 database, which already has it.
DROP INDEX "IX_BaseItems_ExtraType_OwnerId";
DROP INDEX "IX_BaseItems_OwnerId";
CREATE INDEX "IX_BaseItems_ExtraType_OwnerId" ON "BaseItems" ("ExtraType", "OwnerId") WHERE "ExtraType" IS NOT NULL;
CREATE INDEX "IX_BaseItems_OwnerId" ON "BaseItems" ("OwnerId") WHERE "OwnerId" IS NOT NULL;
