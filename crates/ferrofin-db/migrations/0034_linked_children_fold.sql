-- 0034: LinkedChildren becomes the only membership store.
--
-- Runs on EVERY path (never baselined). On an adopted 12.0 database the
-- source table is empty (0009 created it) and this is a no-op; everywhere
-- else it folds Ferrofin's cache table into Jellyfin's 12.0 table, which the
-- code reads from this commit on. One sqlx transaction: any error rolls back
-- the fold AND the drop, and the boot retries.

-- ── LinkedChildren becomes the only membership store ─────────────────────
-- Rows whose parent or child no longer exists are what Jellyfin's own
-- MigrateLinkedChildren.CleanupOrphanedLinkedChildren removes; with
-- foreign_keys = OFF on the migration connection they would otherwise slip
-- through the fold and fail the boot's foreign_key_check.
DELETE FROM "FerrofinLinkedChildren"
 WHERE "ChildId"  NOT IN (SELECT "Id" FROM "BaseItems")
    OR "ParentId" NOT IN (SELECT "Id" FROM "BaseItems");
-- The legacy key was (ParentId, ChildId) with a nullable SortOrder; 12.0's key
-- is (ParentId, SortOrder). Every row gets a fresh, collision-free ordinal —
-- existing order kept, nulls last, rowid as the tiebreak — so the copy cannot
-- lose a row, and a plain INSERT means any collision aborts instead of hiding.
-- On an adopted 12.0 database the source table is empty (0009 created it) and
-- this is a no-op.
INSERT INTO "LinkedChildren" ("ParentId", "SortOrder", "ChildId", "ChildType")
SELECT "ParentId",
       ROW_NUMBER() OVER (PARTITION BY "ParentId" ORDER BY "SortOrder" IS NULL, "SortOrder", "rowid") - 1,
       "ChildId", "ChildType"
  FROM "FerrofinLinkedChildren";
DROP TABLE "FerrofinLinkedChildren";

