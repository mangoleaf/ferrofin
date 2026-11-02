-- 0044: remove rows that name a user, or an ancestry item, that no longer exists.
--
-- Long-lived Jellyfin databases carry them: rows of a deleted user left behind in
-- Permissions, Preferences, UserData, Devices and the display-preference tables, and
-- AncestorIds rows whose item is gone (upstream issues 17830, 17863, 17874, 17875).
-- Jellyfin never runs PRAGMA foreign_key_check, so it boots on them; Ferrofin's boot
-- check (database.rs, foreign_key_check) refuses the database instead.
--
-- Upstream part, verbatim from Jellyfin 12.1 (PR 17835, unchanged in v12.2):
-- src/Jellyfin.Database/Jellyfin.Database.Providers.Sqlite/Migrations/
-- 20260815063607_RemoveOrphanedUserPermissionsAndPreferences.cs:14-15. 0032 ports
-- only its 12.0 form ("UserId" IS NULL) and is released, so the 12.1 form lands here.
--
-- Ferrofin part (owner decision on the adoption-realism milestone, option (a)): delete
-- what ON DELETE CASCADE would have removed when the user or item was deleted, for
-- every other foreign key to "Users" and for "AncestorIds" (upstream 12.1 instead skips
-- ancestry of missing items in ConsolidateLocalizedUserViews.MoveAncestorsAsync and
-- runs with the rest). "CustomItemDisplayPreferences" has no foreign key but is
-- per-user data of the same kind, so it goes too. Migrations run with
-- foreign_keys = OFF (database.rs), so children of the deleted rows are cleared by
-- hand: "HomeSection" under "DisplayPreferences", and the retention snapshot keys
-- under their snapshots. user_cleanup.rs checks this list against the live schema.
--
-- Each delete records its row count in the temporary table below; database.rs logs
-- the counts after the migrator returns and drops it. Never baselined; a no-op on a
-- clean database.

DROP TABLE IF EXISTS temp."FerrofinDanglingRowCleanup";
CREATE TEMPORARY TABLE "FerrofinDanglingRowCleanup" ("Table" TEXT NOT NULL, "Rows" INTEGER NOT NULL);

DELETE FROM "Permissions" WHERE "UserId" IS NULL OR "UserId" NOT IN (SELECT "Id" FROM "Users");
INSERT INTO "FerrofinDanglingRowCleanup" VALUES ('Permissions', changes());
DELETE FROM "Preferences" WHERE "UserId" IS NULL OR "UserId" NOT IN (SELECT "Id" FROM "Users");
INSERT INTO "FerrofinDanglingRowCleanup" VALUES ('Preferences', changes());

DELETE FROM "UserData" WHERE "UserId" NOT IN (SELECT "Id" FROM "Users");
INSERT INTO "FerrofinDanglingRowCleanup" VALUES ('UserData', changes());
DELETE FROM "Devices" WHERE "UserId" NOT IN (SELECT "Id" FROM "Users");
INSERT INTO "FerrofinDanglingRowCleanup" VALUES ('Devices', changes());
DELETE FROM "AccessSchedules" WHERE "UserId" NOT IN (SELECT "Id" FROM "Users");
INSERT INTO "FerrofinDanglingRowCleanup" VALUES ('AccessSchedules', changes());
DELETE FROM "ImageInfos" WHERE "UserId" NOT IN (SELECT "Id" FROM "Users");
INSERT INTO "FerrofinDanglingRowCleanup" VALUES ('ImageInfos', changes());
DELETE FROM "HomeSection" WHERE "DisplayPreferencesId" IN (
    SELECT "Id" FROM "DisplayPreferences" WHERE "UserId" NOT IN (SELECT "Id" FROM "Users"));
INSERT INTO "FerrofinDanglingRowCleanup" VALUES ('HomeSection', changes());
DELETE FROM "DisplayPreferences" WHERE "UserId" NOT IN (SELECT "Id" FROM "Users");
INSERT INTO "FerrofinDanglingRowCleanup" VALUES ('DisplayPreferences', changes());
DELETE FROM "ItemDisplayPreferences" WHERE "UserId" NOT IN (SELECT "Id" FROM "Users");
INSERT INTO "FerrofinDanglingRowCleanup" VALUES ('ItemDisplayPreferences', changes());
DELETE FROM "CustomItemDisplayPreferences" WHERE "UserId" NOT IN (SELECT "Id" FROM "Users");
INSERT INTO "FerrofinDanglingRowCleanup" VALUES ('CustomItemDisplayPreferences', changes());
DELETE FROM "FerrofinUserDataRetentionKeys" WHERE "SnapshotId" IN (
    SELECT "SnapshotId" FROM "FerrofinUserDataRetentionSnapshots"
    WHERE "UserId" NOT IN (SELECT "Id" FROM "Users"));
INSERT INTO "FerrofinDanglingRowCleanup" VALUES ('FerrofinUserDataRetentionKeys', changes());
DELETE FROM "FerrofinUserDataRetentionSnapshots" WHERE "UserId" NOT IN (SELECT "Id" FROM "Users");
INSERT INTO "FerrofinDanglingRowCleanup" VALUES ('FerrofinUserDataRetentionSnapshots', changes());

DELETE FROM "AncestorIds"
WHERE "ItemId" NOT IN (SELECT "Id" FROM "BaseItems")
   OR "ParentItemId" NOT IN (SELECT "Id" FROM "BaseItems");
INSERT INTO "FerrofinDanglingRowCleanup" VALUES ('AncestorIds', changes());
