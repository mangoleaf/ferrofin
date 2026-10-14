-- 0041: a database from a FRESH Jellyfin 12.x install lacks IX_CustomItemDisplayPreferences_UserId
-- (and so, probably, does any database first created after 2021). Jellyfin's
-- 20210320181425_AddIndexesAndCollations drops it, so a fresh install never has it,
-- while the databases the schema pins (tests/data/jellyfin-*-schema.sql) were dumped from still
-- carry it, and 0007 creates it on every Ferrofin database. 0007 is baselined on adoption, so without
-- this a fresh-install adoption ends one index short of a fresh Ferrofin database. DDL
-- byte-identical to tests/data/jellyfin-12.0-schema.sql (SQLite stores it without the
-- IF NOT EXISTS). Never baselined: a no-op wherever the index already exists.
CREATE INDEX IF NOT EXISTS "IX_CustomItemDisplayPreferences_UserId" ON "CustomItemDisplayPreferences"(
  "UserId"
);
