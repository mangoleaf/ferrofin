-- Rename the Ferrofin-owned schema objects from the project's old name.
--
-- Hermit was this project's previous name; every Ferrofin-own table and index
-- still carried it. Renaming is safe for the Jellyfin drop-in contract because
-- these objects are additive and EF-invisible: Jellyfin ignores tables it does
-- not know about, so a two-way swap keeps working (suite/roundtrip.sh).
--
-- Two constraints shaped this file:
--
--   * Migrations 0001-0013 are NOT edited. sqlx records an applied migration by
--     filename AND checksum in _sqlx_migrations, so touching a shipped migration
--     bricks every existing deployment. Their filenames keep "hermit" forever;
--     only the objects are renamed, here, going forward.
--   * SQLite has no ALTER INDEX ... RENAME. Tables rename in place (references
--     follow automatically), but every index must be dropped and recreated under
--     its new name. That is an index rebuild, not a table rewrite.
--
-- One-way door: a Ferrofin binary older than this migration queries Hermit*
-- names and will not find them. Normal forward-migration semantics, called out
-- in the release notes.


-- ── tables (rename in place; foreign keys follow) ─────────────────────────

ALTER TABLE "HermitLinkedChildren" RENAME TO "FerrofinLinkedChildren";
ALTER TABLE "HermitLiveTvChannels" RENAME TO "FerrofinLiveTvChannels";
ALTER TABLE "HermitLiveTvListingProviders" RENAME TO "FerrofinLiveTvListingProviders";
ALTER TABLE "HermitLiveTvPrograms" RENAME TO "FerrofinLiveTvPrograms";
ALTER TABLE "HermitLiveTvRecordings" RENAME TO "FerrofinLiveTvRecordings";
ALTER TABLE "HermitLiveTvSeriesTimers" RENAME TO "FerrofinLiveTvSeriesTimers";
ALTER TABLE "HermitLiveTvTimers" RENAME TO "FerrofinLiveTvTimers";
ALTER TABLE "HermitLiveTvTunerHosts" RENAME TO "FerrofinLiveTvTunerHosts";
ALTER TABLE "HermitMeta" RENAME TO "FerrofinMeta";
ALTER TABLE "HermitPlaybackSessions" RENAME TO "FerrofinPlaybackSessions";
ALTER TABLE "HermitPlaylistShares" RENAME TO "FerrofinPlaylistShares";
ALTER TABLE "HermitPlaylists" RENAME TO "FerrofinPlaylists";

-- ── indexes (no ALTER INDEX RENAME in SQLite: drop + recreate) ───────────

DROP INDEX IF EXISTS "HermitIX_BaseItemImageInfos_ItemId_ImageType";
CREATE INDEX IF NOT EXISTS "FerrofinIX_BaseItemImageInfos_ItemId_ImageType" ON "BaseItemImageInfos" ("ItemId", "ImageType");
DROP INDEX IF EXISTS "HermitIX_BaseItemProviders_ProviderId_ItemId_ProviderValue";
CREATE INDEX IF NOT EXISTS "FerrofinIX_BaseItemProviders_ProviderId_ItemId_ProviderValue" ON "BaseItemProviders" ("ProviderId", "ItemId", "ProviderValue");
DROP INDEX IF EXISTS "HermitIX_BaseItems_ExtraType_OwnerId";
CREATE INDEX IF NOT EXISTS "FerrofinIX_BaseItems_ExtraType_OwnerId" ON "BaseItems" ("ExtraType", "OwnerId");
DROP INDEX IF EXISTS "HermitIX_BaseItems_Name";
CREATE INDEX IF NOT EXISTS "FerrofinIX_BaseItems_Name" ON "BaseItems" ("Name");
DROP INDEX IF EXISTS "HermitIX_BaseItems_OwnerId";
CREATE INDEX IF NOT EXISTS "FerrofinIX_BaseItems_OwnerId" ON "BaseItems" ("OwnerId");
DROP INDEX IF EXISTS "HermitIX_BaseItems_PrimaryVersionId";
CREATE INDEX IF NOT EXISTS "FerrofinIX_BaseItems_PrimaryVersionId" ON "BaseItems" ("PrimaryVersionId") WHERE "PrimaryVersionId" IS NOT NULL;
DROP INDEX IF EXISTS "HermitIX_BaseItems_SeasonId";
CREATE INDEX IF NOT EXISTS "FerrofinIX_BaseItems_SeasonId" ON "BaseItems" ("SeasonId");
DROP INDEX IF EXISTS "HermitIX_BaseItems_SeriesId";
CREATE INDEX IF NOT EXISTS "FerrofinIX_BaseItems_SeriesId" ON "BaseItems" ("SeriesId");
DROP INDEX IF EXISTS "HermitIX_BaseItems_SeriesName";
CREATE INDEX IF NOT EXISTS "FerrofinIX_BaseItems_SeriesName" ON "BaseItems" ("SeriesName");
DROP INDEX IF EXISTS "HermitIX_BaseItems_TopParentId_IsFolder_IsVirtualItem_DateCreated";
CREATE INDEX IF NOT EXISTS "FerrofinIX_BaseItems_TopParentId_IsFolder_IsVirtualItem_DateCreated" ON "BaseItems" ("TopParentId", "IsFolder", "IsVirtualItem", "DateCreated");
DROP INDEX IF EXISTS "HermitIX_BaseItems_TopParentId_MediaType_IsVirtualItem_DateCreated";
CREATE INDEX IF NOT EXISTS "FerrofinIX_BaseItems_TopParentId_MediaType_IsVirtualItem_DateCreated" ON "BaseItems" ("TopParentId", "MediaType", "IsVirtualItem", "DateCreated");
DROP INDEX IF EXISTS "HermitIX_BaseItems_TopParentId_Type_IsVirtualItem";
CREATE INDEX IF NOT EXISTS "FerrofinIX_BaseItems_TopParentId_Type_IsVirtualItem" ON "BaseItems" ("TopParentId", "Type", "IsVirtualItem") WHERE "PrimaryVersionId" IS NULL AND ("OwnerId" IS NULL OR "ExtraType" IS NOT NULL);
DROP INDEX IF EXISTS "HermitIX_BaseItems_TopParentId_Type_IsVirtualItem_DateCreated";
CREATE INDEX IF NOT EXISTS "FerrofinIX_BaseItems_TopParentId_Type_IsVirtualItem_DateCreated" ON "BaseItems" ("TopParentId", "Type", "IsVirtualItem", "DateCreated");
DROP INDEX IF EXISTS "HermitIX_BaseItems_Type_CleanName";
CREATE INDEX IF NOT EXISTS "FerrofinIX_BaseItems_Type_CleanName" ON "BaseItems" ("Type", "CleanName");
DROP INDEX IF EXISTS "HermitIX_BaseItems_Type_SeriesPresentationUniqueKey_ParentIndexNumber_IndexNumber";
CREATE INDEX IF NOT EXISTS "FerrofinIX_BaseItems_Type_SeriesPresentationUniqueKey_ParentIndexNumber_IndexNumber" ON "BaseItems" ("Type", "SeriesPresentationUniqueKey", "ParentIndexNumber", "IndexNumber");
DROP INDEX IF EXISTS "HermitIX_BaseItems_Type_TopParentId_SortName";
CREATE INDEX IF NOT EXISTS "FerrofinIX_BaseItems_Type_TopParentId_SortName" ON "BaseItems" ("Type", "TopParentId", "SortName");
DROP INDEX IF EXISTS "HermitIX_LinkedChildren_ChildId_ChildType";
CREATE INDEX IF NOT EXISTS "FerrofinIX_LinkedChildren_ChildId_ChildType" ON "FerrofinLinkedChildren" ("ChildId", "ChildType");
DROP INDEX IF EXISTS "HermitIX_LinkedChildren_ParentId_ChildType";
CREATE INDEX IF NOT EXISTS "FerrofinIX_LinkedChildren_ParentId_ChildType" ON "FerrofinLinkedChildren" ("ParentId", "ChildType");
DROP INDEX IF EXISTS "HermitIX_LinkedChildren_ParentId_SortOrder";
CREATE INDEX IF NOT EXISTS "FerrofinIX_LinkedChildren_ParentId_SortOrder" ON "FerrofinLinkedChildren" ("ParentId", "SortOrder");
DROP INDEX IF EXISTS "HermitIX_LiveTvChannels_TunerHostId";
CREATE INDEX IF NOT EXISTS "FerrofinIX_LiveTvChannels_TunerHostId" ON "FerrofinLiveTvChannels" ("TunerHostId");
DROP INDEX IF EXISTS "HermitIX_LiveTvChannels_TvgId";
CREATE INDEX IF NOT EXISTS "FerrofinIX_LiveTvChannels_TvgId" ON "FerrofinLiveTvChannels" ("TvgId");
DROP INDEX IF EXISTS "HermitIX_LiveTvPrograms_ChannelId_StartDate";
CREATE INDEX IF NOT EXISTS "FerrofinIX_LiveTvPrograms_ChannelId_StartDate" ON "FerrofinLiveTvPrograms" ("ChannelId", "StartDate");
DROP INDEX IF EXISTS "HermitIX_LiveTvRecordings_ChannelId";
CREATE INDEX IF NOT EXISTS "FerrofinIX_LiveTvRecordings_ChannelId" ON "FerrofinLiveTvRecordings" ("ChannelId");
DROP INDEX IF EXISTS "HermitIX_LiveTvTimers_SeriesTimerId";
CREATE INDEX IF NOT EXISTS "FerrofinIX_LiveTvTimers_SeriesTimerId" ON "FerrofinLiveTvTimers" ("SeriesTimerId");
DROP INDEX IF EXISTS "HermitIX_LiveTvTimers_StartDate";
CREATE INDEX IF NOT EXISTS "FerrofinIX_LiveTvTimers_StartDate" ON "FerrofinLiveTvTimers" ("StartDate");
DROP INDEX IF EXISTS "HermitIX_Peoples_LowerName_Cover";
CREATE INDEX IF NOT EXISTS "FerrofinIX_Peoples_LowerName_Cover" ON "Peoples" (LOWER("Name"), "Name", "PersonType", "Id");
DROP INDEX IF EXISTS "HermitIX_PlaybackSessions_DecidedAt";
CREATE INDEX IF NOT EXISTS "FerrofinIX_PlaybackSessions_DecidedAt" ON "FerrofinPlaybackSessions" ("DecidedAt");
DROP INDEX IF EXISTS "HermitIX_UserData_UserId_IsFavorite_ItemId";
CREATE INDEX IF NOT EXISTS "FerrofinIX_UserData_UserId_IsFavorite_ItemId" ON "UserData" ("UserId", "IsFavorite", "ItemId");
DROP INDEX IF EXISTS "HermitIX_UserData_UserId_ItemId_LastPlayedDate";
CREATE INDEX IF NOT EXISTS "FerrofinIX_UserData_UserId_ItemId_LastPlayedDate" ON "UserData" ("UserId", "ItemId", "LastPlayedDate");
DROP INDEX IF EXISTS "HermitIX_UserData_UserId_Played_ItemId";
CREATE INDEX IF NOT EXISTS "FerrofinIX_UserData_UserId_Played_ItemId" ON "UserData" ("UserId", "Played", "ItemId");
