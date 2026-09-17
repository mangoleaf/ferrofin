-- 0033: Ferrofin-side hygiene on the 12.0 shape. Runs on EVERY path — a fresh
-- database, an upgraded install, an adopted 10.11.8 database (after 0032
-- executed) and an adopted 12.0 database (where 0032 was baselined). It is
-- never baselined. (The LinkedChildren fold is 0034, which lands together with
-- the code that reads the new key.)

-- ── Indexes 12.0 dropped that Ferrofin's queries still plan on (D4) ─────
CREATE INDEX IF NOT EXISTS "FerrofinIX_UserData_UserId" ON "UserData" ("UserId");
CREATE INDEX IF NOT EXISTS "FerrofinIX_PeopleBaseItemMap_PeopleId" ON "PeopleBaseItemMap" ("PeopleId");

-- ── Planner hygiene ──────────────────────────────────────────────────────
-- Jellyfin runs ANALYZE; Ferrofin's pinned plans assume sqlite_stat1 never
-- exists (the index-planner rule in CLAUDE.md).
DROP TABLE IF EXISTS "sqlite_stat1";
-- IX_Peoples_NameLower is an expression index on lower("Name"). A 12.0
-- database built its keys with .NET's Unicode lower(); SQLite's built-in
-- lower() is ASCII-only, so every non-ASCII name is "missing from index".
-- Rebuilding under the bundled SQLite makes the keys consistent with every
-- connection Ferrofin will ever open. Ferrofin never queries through it.
REINDEX "IX_Peoples_NameLower";
