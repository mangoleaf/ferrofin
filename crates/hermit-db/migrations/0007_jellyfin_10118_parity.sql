-- 0007: pin the schema to Jellyfin 10.11.8 (drop-in requirement).
--
-- Hermit 0001 was transliterated from the v12.0-rc3 EF model snapshot; real
-- 10.11.8 databases differ (evidence: the committed schema fixture at
-- crates/hermit-db/tests/data/jellyfin-10.11.8-schema.sql).
-- This migration converges every Jellyfin-owned table to the exact shape a
-- fresh 10.11.8 server creates, renames Hermit-invented tables/indexes out of
-- upstream's namespace (Hermit*/HermitIX_*), and normalizes stored GUID casing
-- (uppercase hyphenated) and datetime text (space-separated, no offset) to
-- Jellyfin's formats. Adopted Jellyfin DBs never run this file — the adoption
-- path baselines it as already applied.
--
-- Generated from a real jellyfin/jellyfin:10.11.8 database; the DDL below is
-- byte-derived from its sqlite_master, not hand-written.
--
-- sqlx's SQLite driver always wraps a migration in ONE transaction (it
-- ignores the `-- no-transaction` marker), so the table rebuilds below rely
-- on defer_foreign_keys: FK checks move to COMMIT, by which point every
-- dropped-and-renamed table exists again. The pragma resets itself at
-- commit.

PRAGMA defer_foreign_keys = ON;
-- ── Users: rebuild to the 10.11.8 shape ─────────────────────────────
CREATE TABLE "Users_jf" (
    "Id" TEXT NOT NULL CONSTRAINT "PK_Users" PRIMARY KEY,
    "AudioLanguagePreference" TEXT NULL,
    "AuthenticationProviderId" TEXT NOT NULL,
    "CastReceiverId" TEXT NULL,
    "DisplayCollectionsView" INTEGER NOT NULL,
    "DisplayMissingEpisodes" INTEGER NOT NULL,
    "EnableAutoLogin" INTEGER NOT NULL,
    "EnableLocalPassword" INTEGER NOT NULL,
    "EnableNextEpisodeAutoPlay" INTEGER NOT NULL,
    "EnableUserPreferenceAccess" INTEGER NOT NULL,
    "HidePlayedInLatest" INTEGER NOT NULL,
    "InternalId" INTEGER NOT NULL,
    "InvalidLoginAttemptCount" INTEGER NOT NULL,
    "LastActivityDate" TEXT NULL,
    "LastLoginDate" TEXT NULL,
    "LoginAttemptsBeforeLockout" INTEGER NULL,
    "MaxActiveSessions" INTEGER NOT NULL,
    "MaxParentalRatingScore" INTEGER NULL,
    "MustUpdatePassword" INTEGER NOT NULL,
    "Password" TEXT NULL,
    "PasswordResetProviderId" TEXT NOT NULL,
    "PlayDefaultAudioTrack" INTEGER NOT NULL,
    "RememberAudioSelections" INTEGER NOT NULL,
    "RememberSubtitleSelections" INTEGER NOT NULL,
    "RemoteClientBitrateLimit" INTEGER NULL,
    "RowVersion" INTEGER NOT NULL,
    "SubtitleLanguagePreference" TEXT NULL,
    "SubtitleMode" INTEGER NOT NULL,
    "SyncPlayAccess" INTEGER NOT NULL,
    "Username" TEXT NOT NULL
, "MaxParentalRatingSubScore" INTEGER NULL);
INSERT INTO "Users_jf" ("Id", "AudioLanguagePreference", "AuthenticationProviderId", "CastReceiverId", "DisplayCollectionsView", "DisplayMissingEpisodes", "EnableAutoLogin", "EnableLocalPassword", "EnableNextEpisodeAutoPlay", "EnableUserPreferenceAccess", "HidePlayedInLatest", "InternalId", "InvalidLoginAttemptCount", "LastActivityDate", "LastLoginDate", "LoginAttemptsBeforeLockout", "MaxActiveSessions", "MaxParentalRatingScore", "MustUpdatePassword", "Password", "PasswordResetProviderId", "PlayDefaultAudioTrack", "RememberAudioSelections", "RememberSubtitleSelections", "RemoteClientBitrateLimit", "RowVersion", "SubtitleLanguagePreference", "SubtitleMode", "SyncPlayAccess", "Username", "MaxParentalRatingSubScore")
SELECT "Id", "AudioLanguagePreference", "AuthenticationProviderId", "CastReceiverId", "DisplayCollectionsView", "DisplayMissingEpisodes", "EnableAutoLogin", "EnableLocalPassword", "EnableNextEpisodeAutoPlay", "EnableUserPreferenceAccess", "HidePlayedInLatest", "InternalId", "InvalidLoginAttemptCount", "LastActivityDate", "LastLoginDate", "LoginAttemptsBeforeLockout", "MaxActiveSessions", "MaxParentalRatingScore", "MustUpdatePassword", "Password", "PasswordResetProviderId", "PlayDefaultAudioTrack", "RememberAudioSelections", "RememberSubtitleSelections", "RemoteClientBitrateLimit", "RowVersion", "SubtitleLanguagePreference", "SubtitleMode", "SyncPlayAccess", "Username", "MaxParentalRatingSubScore" FROM "Users";
DROP TABLE "Users";
ALTER TABLE "Users_jf" RENAME TO "Users";
CREATE UNIQUE INDEX "IX_Users_Username" ON "Users" ("Username");

-- ── BaseItems: rebuild to the 10.11.8 shape ─────────────────────────────
CREATE TABLE "BaseItems_jf" (
    "Id" TEXT NOT NULL CONSTRAINT "PK_BaseItems" PRIMARY KEY,
    "Album" TEXT NULL,
    "AlbumArtists" TEXT NULL,
    "Artists" TEXT NULL,
    "Audio" INTEGER NULL,
    "ChannelId" TEXT NULL,
    "CleanName" TEXT NULL,
    "CommunityRating" REAL NULL,
    "CriticRating" REAL NULL,
    "CustomRating" TEXT NULL,
    "Data" TEXT NULL,
    "DateCreated" TEXT NULL,
    "DateLastMediaAdded" TEXT NULL,
    "DateLastRefreshed" TEXT NULL,
    "DateLastSaved" TEXT NULL,
    "DateModified" TEXT NULL,
    "EndDate" TEXT NULL,
    "EpisodeTitle" TEXT NULL,
    "ExternalId" TEXT NULL,
    "ExternalSeriesId" TEXT NULL,
    "ExternalServiceId" TEXT NULL,
    "ExtraIds" TEXT NULL,
    "ExtraType" INTEGER NULL,
    "ForcedSortName" TEXT NULL,
    "Genres" TEXT NULL,
    "Height" INTEGER NULL,
    "IndexNumber" INTEGER NULL,
    "InheritedParentalRatingSubValue" INTEGER NULL,
    "InheritedParentalRatingValue" INTEGER NULL,
    "IsFolder" INTEGER NOT NULL,
    "IsInMixedFolder" INTEGER NOT NULL,
    "IsLocked" INTEGER NOT NULL,
    "IsMovie" INTEGER NOT NULL,
    "IsRepeat" INTEGER NOT NULL,
    "IsSeries" INTEGER NOT NULL,
    "IsVirtualItem" INTEGER NOT NULL,
    "LUFS" REAL NULL,
    "MediaType" TEXT NULL,
    "Name" TEXT NULL,
    "NormalizationGain" REAL NULL,
    "OfficialRating" TEXT NULL,
    "OriginalTitle" TEXT NULL,
    "Overview" TEXT NULL,
    "OwnerId" TEXT NULL,
    "ParentId" TEXT NULL,
    "ParentIndexNumber" INTEGER NULL,
    "Path" TEXT NULL,
    "PreferredMetadataCountryCode" TEXT NULL,
    "PreferredMetadataLanguage" TEXT NULL,
    "PremiereDate" TEXT NULL,
    "PresentationUniqueKey" TEXT NULL,
    "PrimaryVersionId" TEXT NULL,
    "ProductionLocations" TEXT NULL,
    "ProductionYear" INTEGER NULL,
    "RunTimeTicks" INTEGER NULL,
    "SeasonId" TEXT NULL,
    "SeasonName" TEXT NULL,
    "SeriesId" TEXT NULL,
    "SeriesName" TEXT NULL,
    "SeriesPresentationUniqueKey" TEXT NULL,
    "ShowId" TEXT NULL,
    "Size" INTEGER NULL,
    "SortName" TEXT NULL,
    "StartDate" TEXT NULL,
    "Studios" TEXT NULL,
    "Tagline" TEXT NULL,
    "Tags" TEXT NULL,
    "TopParentId" TEXT NULL,
    "TotalBitrate" INTEGER NULL,
    "Type" TEXT NOT NULL,
    "UnratedType" TEXT NULL,
    "Width" INTEGER NULL,
    CONSTRAINT "FK_BaseItems_BaseItems_ParentId" FOREIGN KEY ("ParentId") REFERENCES "BaseItems" ("Id") ON DELETE CASCADE
);
INSERT INTO "BaseItems_jf" ("Id", "Album", "AlbumArtists", "Artists", "Audio", "ChannelId", "CleanName", "CommunityRating", "CriticRating", "CustomRating", "Data", "DateCreated", "DateLastMediaAdded", "DateLastRefreshed", "DateLastSaved", "DateModified", "EndDate", "EpisodeTitle", "ExternalId", "ExternalSeriesId", "ExternalServiceId", "ExtraType", "ForcedSortName", "Genres", "Height", "IndexNumber", "InheritedParentalRatingSubValue", "InheritedParentalRatingValue", "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie", "IsRepeat", "IsSeries", "IsVirtualItem", "LUFS", "MediaType", "Name", "NormalizationGain", "OfficialRating", "OriginalTitle", "Overview", "OwnerId", "ParentId", "ParentIndexNumber", "Path", "PreferredMetadataCountryCode", "PreferredMetadataLanguage", "PremiereDate", "PresentationUniqueKey", "PrimaryVersionId", "ProductionLocations", "ProductionYear", "RunTimeTicks", "SeasonId", "SeasonName", "SeriesId", "SeriesName", "SeriesPresentationUniqueKey", "ShowId", "Size", "SortName", "StartDate", "Studios", "Tagline", "Tags", "TopParentId", "TotalBitrate", "Type", "UnratedType", "Width")
SELECT "Id", "Album", "AlbumArtists", "Artists", "Audio", "ChannelId", "CleanName", "CommunityRating", "CriticRating", "CustomRating", "Data", "DateCreated", "DateLastMediaAdded", "DateLastRefreshed", "DateLastSaved", "DateModified", "EndDate", "EpisodeTitle", "ExternalId", "ExternalSeriesId", "ExternalServiceId", "ExtraType", "ForcedSortName", "Genres", "Height", "IndexNumber", "InheritedParentalRatingSubValue", "InheritedParentalRatingValue", "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie", "IsRepeat", "IsSeries", "IsVirtualItem", "LUFS", "MediaType", "Name", "NormalizationGain", "OfficialRating", "OriginalTitle", "Overview", "OwnerId", "ParentId", "ParentIndexNumber", "Path", "PreferredMetadataCountryCode", "PreferredMetadataLanguage", "PremiereDate", "PresentationUniqueKey", "PrimaryVersionId", "ProductionLocations", "ProductionYear", "RunTimeTicks", "SeasonId", "SeasonName", "SeriesId", "SeriesName", "SeriesPresentationUniqueKey", "ShowId", "Size", "SortName", "StartDate", "Studios", "Tagline", "Tags", "TopParentId", "TotalBitrate", "Type", "UnratedType", "Width" FROM "BaseItems";
DROP TABLE "BaseItems";
ALTER TABLE "BaseItems_jf" RENAME TO "BaseItems";
CREATE INDEX "IX_BaseItems_Id_Type_IsFolder_IsVirtualItem" ON "BaseItems" ("Id", "Type", "IsFolder", "IsVirtualItem");
CREATE INDEX "IX_BaseItems_IsFolder_TopParentId_IsVirtualItem_PresentationUniqueKey_DateCreated" ON "BaseItems" ("IsFolder", "TopParentId", "IsVirtualItem", "PresentationUniqueKey", "DateCreated");
CREATE INDEX "IX_BaseItems_MediaType_TopParentId_IsVirtualItem_PresentationUniqueKey" ON "BaseItems" ("MediaType", "TopParentId", "IsVirtualItem", "PresentationUniqueKey");
CREATE INDEX "IX_BaseItems_ParentId" ON "BaseItems" ("ParentId");
CREATE INDEX "IX_BaseItems_Path" ON "BaseItems" ("Path");
CREATE INDEX "IX_BaseItems_PresentationUniqueKey" ON "BaseItems" ("PresentationUniqueKey");
CREATE INDEX "IX_BaseItems_TopParentId_Id" ON "BaseItems" ("TopParentId", "Id");
CREATE INDEX "IX_BaseItems_Type_SeriesPresentationUniqueKey_IsFolder_IsVirtualItem" ON "BaseItems" ("Type", "SeriesPresentationUniqueKey", "IsFolder", "IsVirtualItem");
CREATE INDEX "IX_BaseItems_Type_SeriesPresentationUniqueKey_PresentationUniqueKey_SortName" ON "BaseItems" ("Type", "SeriesPresentationUniqueKey", "PresentationUniqueKey", "SortName");
CREATE INDEX "IX_BaseItems_Type_TopParentId_Id" ON "BaseItems" ("Type", "TopParentId", "Id");
CREATE INDEX "IX_BaseItems_Type_TopParentId_IsVirtualItem_PresentationUniqueKey_DateCreated" ON "BaseItems" ("Type", "TopParentId", "IsVirtualItem", "PresentationUniqueKey", "DateCreated");
CREATE INDEX "IX_BaseItems_Type_TopParentId_PresentationUniqueKey" ON "BaseItems" ("Type", "TopParentId", "PresentationUniqueKey");
CREATE INDEX "IX_BaseItems_Type_TopParentId_StartDate" ON "BaseItems" ("Type", "TopParentId", "StartDate");
CREATE INDEX "HermitIX_BaseItems_ExtraType_OwnerId" ON "BaseItems" ("ExtraType", "OwnerId");
CREATE INDEX "HermitIX_BaseItems_Name" ON "BaseItems" ("Name");
CREATE INDEX "HermitIX_BaseItems_OwnerId" ON "BaseItems" ("OwnerId");
CREATE INDEX "HermitIX_BaseItems_SeasonId" ON "BaseItems" ("SeasonId");
CREATE INDEX "HermitIX_BaseItems_SeriesId" ON "BaseItems" ("SeriesId");
CREATE INDEX "HermitIX_BaseItems_SeriesName" ON "BaseItems" ("SeriesName");
CREATE INDEX "HermitIX_BaseItems_TopParentId_IsFolder_IsVirtualItem_DateCreated" ON "BaseItems" ("TopParentId", "IsFolder", "IsVirtualItem", "DateCreated");
CREATE INDEX "HermitIX_BaseItems_TopParentId_MediaType_IsVirtualItem_DateCreated" ON "BaseItems" ("TopParentId", "MediaType", "IsVirtualItem", "DateCreated");
CREATE INDEX "HermitIX_BaseItems_TopParentId_Type_IsVirtualItem" ON "BaseItems" ("TopParentId", "Type", "IsVirtualItem") WHERE "PrimaryVersionId" IS NULL AND ("OwnerId" IS NULL OR "ExtraType" IS NOT NULL);
CREATE INDEX "HermitIX_BaseItems_TopParentId_Type_IsVirtualItem_DateCreated" ON "BaseItems" ("TopParentId", "Type", "IsVirtualItem", "DateCreated");
CREATE INDEX "HermitIX_BaseItems_Type_CleanName" ON "BaseItems" ("Type", "CleanName");
CREATE INDEX "HermitIX_BaseItems_Type_SeriesPresentationUniqueKey_ParentIndexNumber_IndexNumber" ON "BaseItems" ("Type", "SeriesPresentationUniqueKey", "ParentIndexNumber", "IndexNumber");
CREATE INDEX "HermitIX_BaseItems_Type_TopParentId_SortName" ON "BaseItems" ("Type", "TopParentId", "SortName");

-- ── MediaStreamInfos: rebuild to the 10.11.8 shape ─────────────────────────────
CREATE TABLE "MediaStreamInfos_jf" (
    "ItemId" TEXT NOT NULL,
    "StreamIndex" INTEGER NOT NULL,
    "AspectRatio" TEXT NULL,
    "AverageFrameRate" REAL NULL,
    "BitDepth" INTEGER NULL,
    "BitRate" INTEGER NULL,
    "BlPresentFlag" INTEGER NULL,
    "ChannelLayout" TEXT NULL,
    "Channels" INTEGER NULL,
    "Codec" TEXT NULL,
    "CodecTag" TEXT NULL,
    "CodecTimeBase" TEXT NULL,
    "ColorPrimaries" TEXT NULL,
    "ColorSpace" TEXT NULL,
    "ColorTransfer" TEXT NULL,
    "Comment" TEXT NULL,
    "DvBlSignalCompatibilityId" INTEGER NULL,
    "DvLevel" INTEGER NULL,
    "DvProfile" INTEGER NULL,
    "DvVersionMajor" INTEGER NULL,
    "DvVersionMinor" INTEGER NULL,
    "ElPresentFlag" INTEGER NULL,
    "Height" INTEGER NULL,
    "IsAnamorphic" INTEGER NULL,
    "IsAvc" INTEGER NULL,
    "IsDefault" INTEGER NOT NULL,
    "IsExternal" INTEGER NOT NULL,
    "IsForced" INTEGER NOT NULL,
    "IsHearingImpaired" INTEGER NULL,
    "IsInterlaced" INTEGER NULL,
    "KeyFrames" TEXT NULL,
    "Language" TEXT NULL,
    "Level" REAL NULL,
    "NalLengthSize" TEXT NULL,
    "Path" TEXT NULL,
    "PixelFormat" TEXT NULL,
    "Profile" TEXT NULL,
    "RealFrameRate" REAL NULL,
    "RefFrames" INTEGER NULL,
    "Rotation" INTEGER NULL,
    "RpuPresentFlag" INTEGER NULL,
    "SampleRate" INTEGER NULL,
    "StreamType" INTEGER NOT NULL,
    "TimeBase" TEXT NULL,
    "Title" TEXT NULL,
    "Width" INTEGER NULL, "Hdr10PlusPresentFlag" INTEGER NULL,
    CONSTRAINT "PK_MediaStreamInfos" PRIMARY KEY ("ItemId", "StreamIndex"),
    CONSTRAINT "FK_MediaStreamInfos_BaseItems_ItemId" FOREIGN KEY ("ItemId") REFERENCES "BaseItems" ("Id") ON DELETE CASCADE
);
INSERT INTO "MediaStreamInfos_jf" ("ItemId", "StreamIndex", "AspectRatio", "AverageFrameRate", "BitDepth", "BitRate", "BlPresentFlag", "ChannelLayout", "Channels", "Codec", "CodecTag", "CodecTimeBase", "ColorPrimaries", "ColorSpace", "ColorTransfer", "Comment", "DvBlSignalCompatibilityId", "DvLevel", "DvProfile", "DvVersionMajor", "DvVersionMinor", "ElPresentFlag", "Height", "IsAnamorphic", "IsAvc", "IsDefault", "IsExternal", "IsForced", "IsHearingImpaired", "IsInterlaced", "KeyFrames", "Language", "Level", "NalLengthSize", "Path", "PixelFormat", "Profile", "RealFrameRate", "RefFrames", "Rotation", "RpuPresentFlag", "SampleRate", "StreamType", "TimeBase", "Title", "Width", "Hdr10PlusPresentFlag")
SELECT "ItemId", "StreamIndex", "AspectRatio", "AverageFrameRate", "BitDepth", "BitRate", "BlPresentFlag", "ChannelLayout", "Channels", "Codec", "CodecTag", "CodecTimeBase", "ColorPrimaries", "ColorSpace", "ColorTransfer", "Comment", "DvBlSignalCompatibilityId", "DvLevel", "DvProfile", "DvVersionMajor", "DvVersionMinor", "ElPresentFlag", "Height", "IsAnamorphic", "IsAvc", "IsDefault", "IsExternal", "IsForced", "IsHearingImpaired", "IsInterlaced", "KeyFrames", "Language", "Level", "NalLengthSize", "Path", "PixelFormat", "Profile", "RealFrameRate", "RefFrames", "Rotation", "RpuPresentFlag", "SampleRate", "StreamType", "TimeBase", "Title", "Width", "Hdr10PlusPresentFlag" FROM "MediaStreamInfos";
DROP TABLE "MediaStreamInfos";
ALTER TABLE "MediaStreamInfos_jf" RENAME TO "MediaStreamInfos";
CREATE INDEX "IX_MediaStreamInfos_StreamIndex" ON "MediaStreamInfos" ("StreamIndex");
CREATE INDEX "IX_MediaStreamInfos_StreamIndex_StreamType" ON "MediaStreamInfos" ("StreamIndex", "StreamType");
CREATE INDEX "IX_MediaStreamInfos_StreamIndex_StreamType_Language" ON "MediaStreamInfos" ("StreamIndex", "StreamType", "Language");
CREATE INDEX "IX_MediaStreamInfos_StreamType" ON "MediaStreamInfos" ("StreamType");

-- ── TrickplayInfos: rebuild to the 10.11.8 shape ─────────────────────────────
CREATE TABLE "TrickplayInfos_jf" (
    "ItemId" TEXT NOT NULL,
    "Width" INTEGER NOT NULL,
    "Height" INTEGER NOT NULL,
    "TileWidth" INTEGER NOT NULL,
    "TileHeight" INTEGER NOT NULL,
    "ThumbnailCount" INTEGER NOT NULL,
    "Interval" INTEGER NOT NULL,
    "Bandwidth" INTEGER NOT NULL,
    CONSTRAINT "PK_TrickplayInfos" PRIMARY KEY ("ItemId", "Width")
);
INSERT INTO "TrickplayInfos_jf" ("ItemId", "Width", "Height", "TileWidth", "TileHeight", "ThumbnailCount", "Interval", "Bandwidth")
SELECT "ItemId", "Width", "Height", "TileWidth", "TileHeight", "ThumbnailCount", "Interval", "Bandwidth" FROM "TrickplayInfos";
DROP TABLE "TrickplayInfos";
ALTER TABLE "TrickplayInfos_jf" RENAME TO "TrickplayInfos";

-- ── DisplayPreferences: rebuild to the 10.11.8 shape ─────────────────────────────
CREATE TABLE "DisplayPreferences_jf" (
    "Id" INTEGER NOT NULL CONSTRAINT "PK_DisplayPreferences" PRIMARY KEY AUTOINCREMENT,
    "UserId" TEXT NOT NULL,
    "Client" TEXT NOT NULL,
    "ShowSidebar" INTEGER NOT NULL,
    "ShowBackdrop" INTEGER NOT NULL,
    "ScrollDirection" INTEGER NOT NULL,
    "IndexBy" INTEGER NULL,
    "SkipForwardLength" INTEGER NOT NULL,
    "SkipBackwardLength" INTEGER NOT NULL,
    "ChromecastVersion" INTEGER NOT NULL,
    "EnableNextVideoInfoOverlay" INTEGER NOT NULL,
    "DashboardTheme" TEXT NULL,
    "TvHome" TEXT NULL, "ItemId" TEXT NOT NULL DEFAULT '00000000-0000-0000-0000-000000000000',
    CONSTRAINT "FK_DisplayPreferences_Users_UserId" FOREIGN KEY ("UserId") REFERENCES "Users" ("Id") ON DELETE CASCADE
);
INSERT INTO "DisplayPreferences_jf" ("Id", "UserId", "Client", "ShowSidebar", "ShowBackdrop", "ScrollDirection", "IndexBy", "SkipForwardLength", "SkipBackwardLength", "ChromecastVersion", "EnableNextVideoInfoOverlay", "DashboardTheme", "TvHome", "ItemId")
SELECT "Id", "UserId", "Client", "ShowSidebar", "ShowBackdrop", "ScrollDirection", "IndexBy", "SkipForwardLength", "SkipBackwardLength", "ChromecastVersion", "EnableNextVideoInfoOverlay", "DashboardTheme", "TvHome", "ItemId" FROM "DisplayPreferences";
DROP TABLE "DisplayPreferences";
ALTER TABLE "DisplayPreferences_jf" RENAME TO "DisplayPreferences";
CREATE UNIQUE INDEX "IX_DisplayPreferences_UserId_ItemId_Client" ON "DisplayPreferences" ("UserId", "ItemId", "Client");

-- ── Index alignment on tables that keep their shape ──────────────────
CREATE INDEX "IX_BaseItemImageInfos_ItemId" ON "BaseItemImageInfos" ("ItemId");
CREATE INDEX "IX_BaseItemProviders_ProviderId_ProviderValue_ItemId" ON "BaseItemProviders" ("ProviderId", "ProviderValue", "ItemId");
CREATE INDEX "IX_CustomItemDisplayPreferences_UserId" ON "CustomItemDisplayPreferences" ("UserId");
CREATE INDEX "IX_Devices_DeviceId" ON "Devices" ("DeviceId");
CREATE INDEX "IX_UserData_UserId" ON "UserData" ("UserId");
DROP INDEX "IX_BaseItemImageInfos_ItemId_ImageType";
CREATE INDEX "HermitIX_BaseItemImageInfos_ItemId_ImageType" ON "BaseItemImageInfos" ("ItemId", "ImageType");
DROP INDEX "IX_BaseItemProviders_ProviderId_ItemId_ProviderValue";
CREATE INDEX "HermitIX_BaseItemProviders_ProviderId_ItemId_ProviderValue" ON "BaseItemProviders" ("ProviderId", "ItemId", "ProviderValue");
DROP INDEX "IX_UserData_UserId_IsFavorite_ItemId";
CREATE INDEX "HermitIX_UserData_UserId_IsFavorite_ItemId" ON "UserData" ("UserId", "IsFavorite", "ItemId");
DROP INDEX "IX_UserData_UserId_ItemId_LastPlayedDate";
CREATE INDEX "HermitIX_UserData_UserId_ItemId_LastPlayedDate" ON "UserData" ("UserId", "ItemId", "LastPlayedDate");
DROP INDEX "IX_UserData_UserId_Played_ItemId";
CREATE INDEX "HermitIX_UserData_UserId_Played_ItemId" ON "UserData" ("UserId", "Played", "ItemId");

-- ── Hermit-invented tables move to the Hermit* namespace so a future ──
-- ── Jellyfin upgrade (v12 CREATEs LinkedChildren etc.) can never collide ──
ALTER TABLE "LinkedChildren" RENAME TO "HermitLinkedChildren";
DROP INDEX "IX_LinkedChildren_ChildId_ChildType";
CREATE INDEX "HermitIX_LinkedChildren_ChildId_ChildType" ON "HermitLinkedChildren" ("ChildId", "ChildType");
DROP INDEX "IX_LinkedChildren_ParentId_ChildType";
CREATE INDEX "HermitIX_LinkedChildren_ParentId_ChildType" ON "HermitLinkedChildren" ("ParentId", "ChildType");
DROP INDEX "IX_LinkedChildren_ParentId_SortOrder";
CREATE INDEX "HermitIX_LinkedChildren_ParentId_SortOrder" ON "HermitLinkedChildren" ("ParentId", "SortOrder");
ALTER TABLE "Playlists" RENAME TO "HermitPlaylists";
ALTER TABLE "PlaylistShares" RENAME TO "HermitPlaylistShares";
ALTER TABLE "PlaybackSessions" RENAME TO "HermitPlaybackSessions";
DROP INDEX "IX_PlaybackSessions_DecidedAt";
CREATE INDEX "HermitIX_PlaybackSessions_DecidedAt" ON "HermitPlaybackSessions" ("DecidedAt");
ALTER TABLE "LiveTvChannels" RENAME TO "HermitLiveTvChannels";
DROP INDEX "IX_LiveTvChannels_TunerHostId";
CREATE INDEX "HermitIX_LiveTvChannels_TunerHostId" ON "HermitLiveTvChannels" ("TunerHostId");
DROP INDEX "IX_LiveTvChannels_TvgId";
CREATE INDEX "HermitIX_LiveTvChannels_TvgId" ON "HermitLiveTvChannels" ("TvgId");
ALTER TABLE "LiveTvListingProviders" RENAME TO "HermitLiveTvListingProviders";
ALTER TABLE "LiveTvPrograms" RENAME TO "HermitLiveTvPrograms";
DROP INDEX "IX_LiveTvPrograms_ChannelId_StartDate";
CREATE INDEX "HermitIX_LiveTvPrograms_ChannelId_StartDate" ON "HermitLiveTvPrograms" ("ChannelId", "StartDate");
ALTER TABLE "LiveTvRecordings" RENAME TO "HermitLiveTvRecordings";
DROP INDEX "IX_LiveTvRecordings_ChannelId";
CREATE INDEX "HermitIX_LiveTvRecordings_ChannelId" ON "HermitLiveTvRecordings" ("ChannelId");
ALTER TABLE "LiveTvSeriesTimers" RENAME TO "HermitLiveTvSeriesTimers";
ALTER TABLE "LiveTvTimers" RENAME TO "HermitLiveTvTimers";
DROP INDEX "IX_LiveTvTimers_SeriesTimerId";
CREATE INDEX "HermitIX_LiveTvTimers_SeriesTimerId" ON "HermitLiveTvTimers" ("SeriesTimerId");
DROP INDEX "IX_LiveTvTimers_StartDate";
CREATE INDEX "HermitIX_LiveTvTimers_StartDate" ON "HermitLiveTvTimers" ("StartDate");
ALTER TABLE "LiveTvTunerHosts" RENAME TO "HermitLiveTvTunerHosts";

-- ── GUID casing: Jellyfin stores uppercase hyphenated; guard on shape ──
UPDATE "Users" SET "Id" = UPPER("Id") WHERE "Id" IS NOT NULL AND length("Id") = 36 AND substr("Id", 9, 1) = '-';
UPDATE "Permissions" SET "UserId" = UPPER("UserId") WHERE "UserId" IS NOT NULL AND length("UserId") = 36 AND substr("UserId", 9, 1) = '-';
UPDATE "Preferences" SET "UserId" = UPPER("UserId") WHERE "UserId" IS NOT NULL AND length("UserId") = 36 AND substr("UserId", 9, 1) = '-';
UPDATE "AccessSchedules" SET "UserId" = UPPER("UserId") WHERE "UserId" IS NOT NULL AND length("UserId") = 36 AND substr("UserId", 9, 1) = '-';
UPDATE "ImageInfos" SET "UserId" = UPPER("UserId") WHERE "UserId" IS NOT NULL AND length("UserId") = 36 AND substr("UserId", 9, 1) = '-';
UPDATE "ActivityLogs" SET "UserId" = UPPER("UserId") WHERE "UserId" IS NOT NULL AND length("UserId") = 36 AND substr("UserId", 9, 1) = '-';
UPDATE "Devices" SET "UserId" = UPPER("UserId") WHERE "UserId" IS NOT NULL AND length("UserId") = 36 AND substr("UserId", 9, 1) = '-';
UPDATE "DisplayPreferences" SET "UserId" = UPPER("UserId") WHERE "UserId" IS NOT NULL AND length("UserId") = 36 AND substr("UserId", 9, 1) = '-';
UPDATE "DisplayPreferences" SET "ItemId" = UPPER("ItemId") WHERE "ItemId" IS NOT NULL AND length("ItemId") = 36 AND substr("ItemId", 9, 1) = '-';
UPDATE "ItemDisplayPreferences" SET "UserId" = UPPER("UserId") WHERE "UserId" IS NOT NULL AND length("UserId") = 36 AND substr("UserId", 9, 1) = '-';
UPDATE "ItemDisplayPreferences" SET "ItemId" = UPPER("ItemId") WHERE "ItemId" IS NOT NULL AND length("ItemId") = 36 AND substr("ItemId", 9, 1) = '-';
UPDATE "CustomItemDisplayPreferences" SET "UserId" = UPPER("UserId") WHERE "UserId" IS NOT NULL AND length("UserId") = 36 AND substr("UserId", 9, 1) = '-';
UPDATE "CustomItemDisplayPreferences" SET "ItemId" = UPPER("ItemId") WHERE "ItemId" IS NOT NULL AND length("ItemId") = 36 AND substr("ItemId", 9, 1) = '-';
UPDATE "BaseItems" SET "Id" = UPPER("Id") WHERE "Id" IS NOT NULL AND length("Id") = 36 AND substr("Id", 9, 1) = '-';
UPDATE "BaseItems" SET "ParentId" = UPPER("ParentId") WHERE "ParentId" IS NOT NULL AND length("ParentId") = 36 AND substr("ParentId", 9, 1) = '-';
UPDATE "BaseItems" SET "OwnerId" = UPPER("OwnerId") WHERE "OwnerId" IS NOT NULL AND length("OwnerId") = 36 AND substr("OwnerId", 9, 1) = '-';
UPDATE "BaseItems" SET "TopParentId" = UPPER("TopParentId") WHERE "TopParentId" IS NOT NULL AND length("TopParentId") = 36 AND substr("TopParentId", 9, 1) = '-';
UPDATE "BaseItems" SET "SeasonId" = UPPER("SeasonId") WHERE "SeasonId" IS NOT NULL AND length("SeasonId") = 36 AND substr("SeasonId", 9, 1) = '-';
UPDATE "BaseItems" SET "SeriesId" = UPPER("SeriesId") WHERE "SeriesId" IS NOT NULL AND length("SeriesId") = 36 AND substr("SeriesId", 9, 1) = '-';
UPDATE "BaseItems" SET "ChannelId" = UPPER("ChannelId") WHERE "ChannelId" IS NOT NULL AND length("ChannelId") = 36 AND substr("ChannelId", 9, 1) = '-';
UPDATE "AncestorIds" SET "ItemId" = UPPER("ItemId") WHERE "ItemId" IS NOT NULL AND length("ItemId") = 36 AND substr("ItemId", 9, 1) = '-';
UPDATE "AncestorIds" SET "ParentItemId" = UPPER("ParentItemId") WHERE "ParentItemId" IS NOT NULL AND length("ParentItemId") = 36 AND substr("ParentItemId", 9, 1) = '-';
UPDATE "AttachmentStreamInfos" SET "ItemId" = UPPER("ItemId") WHERE "ItemId" IS NOT NULL AND length("ItemId") = 36 AND substr("ItemId", 9, 1) = '-';
UPDATE "BaseItemImageInfos" SET "Id" = UPPER("Id") WHERE "Id" IS NOT NULL AND length("Id") = 36 AND substr("Id", 9, 1) = '-';
UPDATE "BaseItemImageInfos" SET "ItemId" = UPPER("ItemId") WHERE "ItemId" IS NOT NULL AND length("ItemId") = 36 AND substr("ItemId", 9, 1) = '-';
UPDATE "BaseItemMetadataFields" SET "ItemId" = UPPER("ItemId") WHERE "ItemId" IS NOT NULL AND length("ItemId") = 36 AND substr("ItemId", 9, 1) = '-';
UPDATE "BaseItemTrailerTypes" SET "ItemId" = UPPER("ItemId") WHERE "ItemId" IS NOT NULL AND length("ItemId") = 36 AND substr("ItemId", 9, 1) = '-';
UPDATE "BaseItemProviders" SET "ItemId" = UPPER("ItemId") WHERE "ItemId" IS NOT NULL AND length("ItemId") = 36 AND substr("ItemId", 9, 1) = '-';
UPDATE "Chapters" SET "ItemId" = UPPER("ItemId") WHERE "ItemId" IS NOT NULL AND length("ItemId") = 36 AND substr("ItemId", 9, 1) = '-';
UPDATE "ItemValues" SET "ItemValueId" = UPPER("ItemValueId") WHERE "ItemValueId" IS NOT NULL AND length("ItemValueId") = 36 AND substr("ItemValueId", 9, 1) = '-';
UPDATE "ItemValuesMap" SET "ItemId" = UPPER("ItemId") WHERE "ItemId" IS NOT NULL AND length("ItemId") = 36 AND substr("ItemId", 9, 1) = '-';
UPDATE "ItemValuesMap" SET "ItemValueId" = UPPER("ItemValueId") WHERE "ItemValueId" IS NOT NULL AND length("ItemValueId") = 36 AND substr("ItemValueId", 9, 1) = '-';
UPDATE "KeyframeData" SET "ItemId" = UPPER("ItemId") WHERE "ItemId" IS NOT NULL AND length("ItemId") = 36 AND substr("ItemId", 9, 1) = '-';
UPDATE "MediaSegments" SET "Id" = UPPER("Id") WHERE "Id" IS NOT NULL AND length("Id") = 36 AND substr("Id", 9, 1) = '-';
UPDATE "MediaSegments" SET "ItemId" = UPPER("ItemId") WHERE "ItemId" IS NOT NULL AND length("ItemId") = 36 AND substr("ItemId", 9, 1) = '-';
UPDATE "MediaStreamInfos" SET "ItemId" = UPPER("ItemId") WHERE "ItemId" IS NOT NULL AND length("ItemId") = 36 AND substr("ItemId", 9, 1) = '-';
UPDATE "PeopleBaseItemMap" SET "ItemId" = UPPER("ItemId") WHERE "ItemId" IS NOT NULL AND length("ItemId") = 36 AND substr("ItemId", 9, 1) = '-';
UPDATE "PeopleBaseItemMap" SET "PeopleId" = UPPER("PeopleId") WHERE "PeopleId" IS NOT NULL AND length("PeopleId") = 36 AND substr("PeopleId", 9, 1) = '-';
UPDATE "Peoples" SET "Id" = UPPER("Id") WHERE "Id" IS NOT NULL AND length("Id") = 36 AND substr("Id", 9, 1) = '-';
UPDATE "TrickplayInfos" SET "ItemId" = UPPER("ItemId") WHERE "ItemId" IS NOT NULL AND length("ItemId") = 36 AND substr("ItemId", 9, 1) = '-';
UPDATE "UserData" SET "ItemId" = UPPER("ItemId") WHERE "ItemId" IS NOT NULL AND length("ItemId") = 36 AND substr("ItemId", 9, 1) = '-';
UPDATE "UserData" SET "UserId" = UPPER("UserId") WHERE "UserId" IS NOT NULL AND length("UserId") = 36 AND substr("UserId", 9, 1) = '-';
UPDATE "HermitLinkedChildren" SET "ParentId" = UPPER("ParentId") WHERE "ParentId" IS NOT NULL AND length("ParentId") = 36 AND substr("ParentId", 9, 1) = '-';
UPDATE "HermitLinkedChildren" SET "ChildId" = UPPER("ChildId") WHERE "ChildId" IS NOT NULL AND length("ChildId") = 36 AND substr("ChildId", 9, 1) = '-';
UPDATE "HermitPlaylists" SET "PlaylistId" = UPPER("PlaylistId") WHERE "PlaylistId" IS NOT NULL AND length("PlaylistId") = 36 AND substr("PlaylistId", 9, 1) = '-';
UPDATE "HermitPlaylists" SET "OwnerUserId" = UPPER("OwnerUserId") WHERE "OwnerUserId" IS NOT NULL AND length("OwnerUserId") = 36 AND substr("OwnerUserId", 9, 1) = '-';
UPDATE "HermitPlaylistShares" SET "PlaylistId" = UPPER("PlaylistId") WHERE "PlaylistId" IS NOT NULL AND length("PlaylistId") = 36 AND substr("PlaylistId", 9, 1) = '-';
UPDATE "HermitPlaylistShares" SET "UserId" = UPPER("UserId") WHERE "UserId" IS NOT NULL AND length("UserId") = 36 AND substr("UserId", 9, 1) = '-';

-- ── Datetimes: RFC3339 ('T' + offset, written by sqlx before 0007) becomes ──
-- ── Jellyfin's 'YYYY-MM-DD HH:MM:SS.SSS' (UTC, no offset). New writes carry ──
-- ── 7 fractional digits; strftime's 3 here are fine (.NET parses any). ──
UPDATE "BaseItems" SET "DateCreated" = COALESCE(strftime('%Y-%m-%d %H:%M:%f', "DateCreated"), "DateCreated") WHERE "DateCreated" IS NOT NULL AND instr("DateCreated", 'T') > 0;
UPDATE "BaseItems" SET "DateLastMediaAdded" = COALESCE(strftime('%Y-%m-%d %H:%M:%f', "DateLastMediaAdded"), "DateLastMediaAdded") WHERE "DateLastMediaAdded" IS NOT NULL AND instr("DateLastMediaAdded", 'T') > 0;
UPDATE "BaseItems" SET "DateLastRefreshed" = COALESCE(strftime('%Y-%m-%d %H:%M:%f', "DateLastRefreshed"), "DateLastRefreshed") WHERE "DateLastRefreshed" IS NOT NULL AND instr("DateLastRefreshed", 'T') > 0;
UPDATE "BaseItems" SET "DateLastSaved" = COALESCE(strftime('%Y-%m-%d %H:%M:%f', "DateLastSaved"), "DateLastSaved") WHERE "DateLastSaved" IS NOT NULL AND instr("DateLastSaved", 'T') > 0;
UPDATE "BaseItems" SET "DateModified" = COALESCE(strftime('%Y-%m-%d %H:%M:%f', "DateModified"), "DateModified") WHERE "DateModified" IS NOT NULL AND instr("DateModified", 'T') > 0;
UPDATE "BaseItems" SET "EndDate" = COALESCE(strftime('%Y-%m-%d %H:%M:%f', "EndDate"), "EndDate") WHERE "EndDate" IS NOT NULL AND instr("EndDate", 'T') > 0;
UPDATE "BaseItems" SET "PremiereDate" = COALESCE(strftime('%Y-%m-%d %H:%M:%f', "PremiereDate"), "PremiereDate") WHERE "PremiereDate" IS NOT NULL AND instr("PremiereDate", 'T') > 0;
UPDATE "BaseItems" SET "StartDate" = COALESCE(strftime('%Y-%m-%d %H:%M:%f', "StartDate"), "StartDate") WHERE "StartDate" IS NOT NULL AND instr("StartDate", 'T') > 0;
UPDATE "Users" SET "LastActivityDate" = COALESCE(strftime('%Y-%m-%d %H:%M:%f', "LastActivityDate"), "LastActivityDate") WHERE "LastActivityDate" IS NOT NULL AND instr("LastActivityDate", 'T') > 0;
UPDATE "Users" SET "LastLoginDate" = COALESCE(strftime('%Y-%m-%d %H:%M:%f', "LastLoginDate"), "LastLoginDate") WHERE "LastLoginDate" IS NOT NULL AND instr("LastLoginDate", 'T') > 0;
UPDATE "Devices" SET "DateCreated" = COALESCE(strftime('%Y-%m-%d %H:%M:%f', "DateCreated"), "DateCreated") WHERE "DateCreated" IS NOT NULL AND instr("DateCreated", 'T') > 0;
UPDATE "Devices" SET "DateModified" = COALESCE(strftime('%Y-%m-%d %H:%M:%f', "DateModified"), "DateModified") WHERE "DateModified" IS NOT NULL AND instr("DateModified", 'T') > 0;
UPDATE "Devices" SET "DateLastActivity" = COALESCE(strftime('%Y-%m-%d %H:%M:%f', "DateLastActivity"), "DateLastActivity") WHERE "DateLastActivity" IS NOT NULL AND instr("DateLastActivity", 'T') > 0;
UPDATE "ActivityLogs" SET "DateCreated" = COALESCE(strftime('%Y-%m-%d %H:%M:%f', "DateCreated"), "DateCreated") WHERE "DateCreated" IS NOT NULL AND instr("DateCreated", 'T') > 0;
UPDATE "ApiKeys" SET "DateCreated" = COALESCE(strftime('%Y-%m-%d %H:%M:%f', "DateCreated"), "DateCreated") WHERE "DateCreated" IS NOT NULL AND instr("DateCreated", 'T') > 0;
UPDATE "ApiKeys" SET "DateLastActivity" = COALESCE(strftime('%Y-%m-%d %H:%M:%f', "DateLastActivity"), "DateLastActivity") WHERE "DateLastActivity" IS NOT NULL AND instr("DateLastActivity", 'T') > 0;
UPDATE "UserData" SET "LastPlayedDate" = COALESCE(strftime('%Y-%m-%d %H:%M:%f', "LastPlayedDate"), "LastPlayedDate") WHERE "LastPlayedDate" IS NOT NULL AND instr("LastPlayedDate", 'T') > 0;
UPDATE "UserData" SET "RetentionDate" = COALESCE(strftime('%Y-%m-%d %H:%M:%f', "RetentionDate"), "RetentionDate") WHERE "RetentionDate" IS NOT NULL AND instr("RetentionDate", 'T') > 0;
UPDATE "BaseItemImageInfos" SET "DateModified" = COALESCE(strftime('%Y-%m-%d %H:%M:%f', "DateModified"), "DateModified") WHERE "DateModified" IS NOT NULL AND instr("DateModified", 'T') > 0;
UPDATE "Chapters" SET "ImageDateModified" = COALESCE(strftime('%Y-%m-%d %H:%M:%f', "ImageDateModified"), "ImageDateModified") WHERE "ImageDateModified" IS NOT NULL AND instr("ImageDateModified", 'T') > 0;

