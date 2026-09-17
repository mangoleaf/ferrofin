-- 0032: converge the Jellyfin-owned schema on Jellyfin 12.0.
--
-- Ferrofin keeps ONE runtime schema. Through 0029 it was byte-equal to a real
-- Jellyfin 10.11.8 database; from here it is byte-equal to a real Jellyfin
-- 12.0.0 database (tests/data/jellyfin-12.0-schema.sql — the sqlite_master of
-- the owner's production database after one jellyfin/jellyfin:12.0 boot, the
-- realistic adoption case; a fresh 12.0 install differs only by lacking
-- IX_CustomItemDisplayPreferences_UserId, which 0007 already creates).
--
-- This file is a port of Jellyfin's nineteen 2026* EF migrations, DDL
-- byte-derived from that fixture, plus the data statements those migrations
-- run. It is BASELINED (recorded, never executed) on an adopted 12.0 database,
-- which already has this shape — so nothing Ferrofin-specific belongs here;
-- that is 0033, which runs on every path.
--
-- Table rebuilds follow 0007's 12-step dance and rely on the boot connection
-- running migrations with foreign_keys = OFF (DROP TABLE would otherwise
-- cascade-delete every child row — see database.rs); the boot then runs
-- PRAGMA foreign_key_check unconditionally. A file snapshot (<db>.pre-0032) is
-- taken before this file first applies.

PRAGMA defer_foreign_keys = ON;

-- ── Data statements, in upstream order (statement-for-statement ports) ───
-- 20260113203012_ChangeOwnerIdToGuid (Migrations/…ChangeOwnerIdToGuid.cs:16-39, 47-52)
UPDATE "BaseItems" SET "OwnerId" = upper("OwnerId") WHERE "OwnerId" IS NOT NULL;
UPDATE "BaseItems" SET "OwnerId" = NULL WHERE "OwnerId" IS NOT NULL AND length("OwnerId") != 36;
UPDATE "BaseItems" SET "OwnerId" = NULL WHERE "OwnerId" = '00000000-0000-0000-0000-000000000000';
UPDATE "BaseItems" SET "OwnerId" = NULL WHERE "OwnerId" = '00000000-0000-0000-0000-000000000001';
UPDATE "BaseItems"
   SET "Name" = 'This is a placeholder item for UserData that has been detached from its original item',
       "OwnerId" = NULL
 WHERE "Id" = '00000000-0000-0000-0000-000000000001';
-- 20260113233000_AddForeignKeyToOwnerId (:26-32): dangling owners are re-pointed at
-- the placeholder item, not nulled — the CleanupOrphanedExtras routine then deletes
-- exactly those items through the library manager (Ferrofin: a boot repair, so the
-- delete cascades through UserData/MediaStreamInfos/AncestorIds like upstream's).
UPDATE "BaseItems" SET "OwnerId" = '00000000-0000-0000-0000-000000000001'
 WHERE "OwnerId" IS NOT NULL AND "OwnerId" NOT IN (SELECT "Id" FROM "BaseItems");
-- 20260215201634_ChangePrimaryVersionIdToGuid (:15-41): N-format → dashed, uppercase,
-- anything not 36 chars → NULL, zero GUID → NULL.
UPDATE "BaseItems"
   SET "PrimaryVersionId" = upper(substr("PrimaryVersionId", 1, 8) || '-' || substr("PrimaryVersionId", 9, 4) || '-'
                             || substr("PrimaryVersionId", 13, 4) || '-' || substr("PrimaryVersionId", 17, 4) || '-'
                             || substr("PrimaryVersionId", 21, 12))
 WHERE "PrimaryVersionId" IS NOT NULL AND length("PrimaryVersionId") = 32;
UPDATE "BaseItems" SET "PrimaryVersionId" = upper("PrimaryVersionId") WHERE "PrimaryVersionId" IS NOT NULL;
UPDATE "BaseItems" SET "PrimaryVersionId" = NULL
 WHERE "PrimaryVersionId" IS NOT NULL AND length("PrimaryVersionId") != 36;
UPDATE "BaseItems" SET "PrimaryVersionId" = NULL
 WHERE "PrimaryVersionId" = '00000000-0000-0000-0000-000000000000';
-- 20260815063607_RemoveOrphanedUserPermissionsAndPreferences (:14-15); UserId becomes
-- NOT NULL in the rebuild below.
DELETE FROM "Permissions" WHERE "UserId" IS NULL;
DELETE FROM "Preferences" WHERE "UserId" IS NULL;
-- 20260522092303/4 NormalizedUsername: `0030` owns the column (or it is
-- baselined where Jellyfin already created it), so the rebuild carries the
-- stored key across unchanged — the ICU backfill that runs after every
-- migration pass has either written it already or writes it right after.

-- ── Users: rebuild to the 12.0 shape ─────────────────────────────
CREATE TABLE "Users_jf"(
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
  ,
  "MaxParentalRatingSubScore" INTEGER NULL,
  "NormalizedUsername" TEXT NOT NULL DEFAULT ''
);
INSERT INTO "Users_jf" ("Id", "AudioLanguagePreference", "AuthenticationProviderId", "CastReceiverId", "DisplayCollectionsView", "DisplayMissingEpisodes", "EnableAutoLogin", "EnableLocalPassword", "EnableNextEpisodeAutoPlay", "EnableUserPreferenceAccess", "HidePlayedInLatest", "InternalId", "InvalidLoginAttemptCount", "LastActivityDate", "LastLoginDate", "LoginAttemptsBeforeLockout", "MaxActiveSessions", "MaxParentalRatingScore", "MustUpdatePassword", "Password", "PasswordResetProviderId", "PlayDefaultAudioTrack", "RememberAudioSelections", "RememberSubtitleSelections", "RemoteClientBitrateLimit", "RowVersion", "SubtitleLanguagePreference", "SubtitleMode", "SyncPlayAccess", "Username", "MaxParentalRatingSubScore", "NormalizedUsername")
SELECT "Id", "AudioLanguagePreference", "AuthenticationProviderId", "CastReceiverId", "DisplayCollectionsView", "DisplayMissingEpisodes", "EnableAutoLogin", "EnableLocalPassword", "EnableNextEpisodeAutoPlay", "EnableUserPreferenceAccess", "HidePlayedInLatest", "InternalId", "InvalidLoginAttemptCount", "LastActivityDate", "LastLoginDate", "LoginAttemptsBeforeLockout", "MaxActiveSessions", "MaxParentalRatingScore", "MustUpdatePassword", "Password", "PasswordResetProviderId", "PlayDefaultAudioTrack", "RememberAudioSelections", "RememberSubtitleSelections", "RemoteClientBitrateLimit", "RowVersion", "SubtitleLanguagePreference", "SubtitleMode", "SyncPlayAccess", "Username", "MaxParentalRatingSubScore", "NormalizedUsername" FROM "Users";
DROP TABLE "Users";
ALTER TABLE "Users_jf" RENAME TO "Users";
CREATE UNIQUE INDEX "IX_Users_NormalizedUsername" ON "Users"(
  "NormalizedUsername"
);
CREATE UNIQUE INDEX "IX_Users_Username" ON "Users"("Username");

-- ── Permissions: rebuild to the 12.0 shape ─────────────────────────────
CREATE TABLE "Permissions_jf"(
  "Id" INTEGER NOT NULL CONSTRAINT "PK_Permissions" PRIMARY KEY AUTOINCREMENT,
  "Kind" INTEGER NOT NULL,
  "RowVersion" INTEGER NOT NULL,
  "UserId" TEXT NOT NULL,
  "Value" INTEGER NOT NULL,
  CONSTRAINT "FK_Permissions_Users_UserId" FOREIGN KEY("UserId") REFERENCES "Users"("Id") ON DELETE CASCADE
);
INSERT INTO "Permissions_jf" ("Id", "Kind", "RowVersion", "UserId", "Value")
SELECT "Id", "Kind", "RowVersion", "UserId", "Value" FROM "Permissions";
DROP TABLE "Permissions";
ALTER TABLE "Permissions_jf" RENAME TO "Permissions";
CREATE UNIQUE INDEX "IX_Permissions_UserId_Kind" ON "Permissions"(
  "UserId",
  "Kind"
);

-- ── Preferences: rebuild to the 12.0 shape ─────────────────────────────
CREATE TABLE "Preferences_jf"(
  "Id" INTEGER NOT NULL CONSTRAINT "PK_Preferences" PRIMARY KEY AUTOINCREMENT,
  "Kind" INTEGER NOT NULL,
  "RowVersion" INTEGER NOT NULL,
  "UserId" TEXT NOT NULL,
  "Value" TEXT NOT NULL,
  CONSTRAINT "FK_Preferences_Users_UserId" FOREIGN KEY("UserId") REFERENCES "Users"("Id") ON DELETE CASCADE
);
INSERT INTO "Preferences_jf" ("Id", "Kind", "RowVersion", "UserId", "Value")
SELECT "Id", "Kind", "RowVersion", "UserId", "Value" FROM "Preferences";
DROP TABLE "Preferences";
ALTER TABLE "Preferences_jf" RENAME TO "Preferences";
CREATE UNIQUE INDEX "IX_Preferences_UserId_Kind" ON "Preferences"(
  "UserId",
  "Kind"
);

-- ── MediaStreamInfos: rebuild to the 12.0 shape ─────────────────────────────
CREATE TABLE "MediaStreamInfos_jf"(
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
  "Width" INTEGER NULL,
  "Hdr10PlusPresentFlag" INTEGER NULL,
  "IsOriginal" INTEGER NOT NULL DEFAULT 0,
  CONSTRAINT "PK_MediaStreamInfos" PRIMARY KEY("ItemId", "StreamIndex"),
  CONSTRAINT "FK_MediaStreamInfos_BaseItems_ItemId" FOREIGN KEY("ItemId") REFERENCES "BaseItems"("Id") ON DELETE CASCADE
);
INSERT INTO "MediaStreamInfos_jf" ("ItemId", "StreamIndex", "AspectRatio", "AverageFrameRate", "BitDepth", "BitRate", "BlPresentFlag", "ChannelLayout", "Channels", "Codec", "CodecTag", "CodecTimeBase", "ColorPrimaries", "ColorSpace", "ColorTransfer", "Comment", "DvBlSignalCompatibilityId", "DvLevel", "DvProfile", "DvVersionMajor", "DvVersionMinor", "ElPresentFlag", "Height", "IsAnamorphic", "IsAvc", "IsDefault", "IsExternal", "IsForced", "IsHearingImpaired", "IsInterlaced", "KeyFrames", "Language", "Level", "NalLengthSize", "Path", "PixelFormat", "Profile", "RealFrameRate", "RefFrames", "Rotation", "RpuPresentFlag", "SampleRate", "StreamType", "TimeBase", "Title", "Width", "Hdr10PlusPresentFlag", "IsOriginal")
SELECT "ItemId", "StreamIndex", "AspectRatio", "AverageFrameRate", "BitDepth", "BitRate", "BlPresentFlag", "ChannelLayout", "Channels", "Codec", "CodecTag", "CodecTimeBase", "ColorPrimaries", "ColorSpace", "ColorTransfer", "Comment", "DvBlSignalCompatibilityId", "DvLevel", "DvProfile", "DvVersionMajor", "DvVersionMinor", "ElPresentFlag", "Height", "IsAnamorphic", "IsAvc", "IsDefault", "IsExternal", "IsForced", "IsHearingImpaired", "IsInterlaced", "KeyFrames", "Language", "Level", "NalLengthSize", "Path", "PixelFormat", "Profile", "RealFrameRate", "RefFrames", "Rotation", "RpuPresentFlag", "SampleRate", "StreamType", "TimeBase", "Title", "Width", "Hdr10PlusPresentFlag", 0 FROM "MediaStreamInfos";
DROP TABLE "MediaStreamInfos";
ALTER TABLE "MediaStreamInfos_jf" RENAME TO "MediaStreamInfos";
CREATE INDEX "IX_MediaStreamInfos_StreamType_ItemId_Language_IsExternal" ON "MediaStreamInfos"(
  "StreamType",
  "ItemId",
  "Language",
  "IsExternal"
);
CREATE INDEX "FerrofinIX_MediaStreamInfos_ItemId_StreamType"
ON "MediaStreamInfos"(
  "ItemId",
  "StreamType"
);

-- ── BaseItems: rebuild to the 12.0 shape ─────────────────────────────
CREATE TABLE "BaseItems_jf"(
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
  "OriginalLanguage" TEXT NULL,
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
  CONSTRAINT "FK_BaseItems_BaseItems_OwnerId" FOREIGN KEY("OwnerId") REFERENCES "BaseItems"("Id"),
  CONSTRAINT "FK_BaseItems_BaseItems_ParentId" FOREIGN KEY("ParentId") REFERENCES "BaseItems"("Id") ON DELETE CASCADE
);
INSERT INTO "BaseItems_jf" ("Id", "Album", "AlbumArtists", "Artists", "Audio", "ChannelId", "CleanName", "CommunityRating", "CriticRating", "CustomRating", "Data", "DateCreated", "DateLastMediaAdded", "DateLastRefreshed", "DateLastSaved", "DateModified", "EndDate", "EpisodeTitle", "ExternalId", "ExternalSeriesId", "ExternalServiceId", "ExtraType", "ForcedSortName", "Genres", "Height", "IndexNumber", "InheritedParentalRatingSubValue", "InheritedParentalRatingValue", "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie", "IsRepeat", "IsSeries", "IsVirtualItem", "LUFS", "MediaType", "Name", "NormalizationGain", "OfficialRating", "OriginalLanguage", "OriginalTitle", "Overview", "OwnerId", "ParentId", "ParentIndexNumber", "Path", "PreferredMetadataCountryCode", "PreferredMetadataLanguage", "PremiereDate", "PresentationUniqueKey", "PrimaryVersionId", "ProductionLocations", "ProductionYear", "RunTimeTicks", "SeasonId", "SeasonName", "SeriesId", "SeriesName", "SeriesPresentationUniqueKey", "ShowId", "Size", "SortName", "StartDate", "Studios", "Tagline", "Tags", "TopParentId", "TotalBitrate", "Type", "UnratedType", "Width")
SELECT "Id", "Album", "AlbumArtists", "Artists", "Audio", "ChannelId", "CleanName", "CommunityRating", "CriticRating", "CustomRating", "Data", "DateCreated", "DateLastMediaAdded", "DateLastRefreshed", "DateLastSaved", "DateModified", "EndDate", "EpisodeTitle", "ExternalId", "ExternalSeriesId", "ExternalServiceId", "ExtraType", "ForcedSortName", "Genres", "Height", "IndexNumber", "InheritedParentalRatingSubValue", "InheritedParentalRatingValue", "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie", "IsRepeat", "IsSeries", "IsVirtualItem", "LUFS", "MediaType", "Name", "NormalizationGain", "OfficialRating", NULL, "OriginalTitle", "Overview", "OwnerId", "ParentId", "ParentIndexNumber", "Path", "PreferredMetadataCountryCode", "PreferredMetadataLanguage", "PremiereDate", "PresentationUniqueKey", "PrimaryVersionId", "ProductionLocations", "ProductionYear", "RunTimeTicks", "SeasonId", "SeasonName", "SeriesId", "SeriesName", "SeriesPresentationUniqueKey", "ShowId", "Size", "SortName", "StartDate", "Studios", "Tagline", "Tags", "TopParentId", "TotalBitrate", "Type", "UnratedType", "Width" FROM "BaseItems";
DROP TABLE "BaseItems";
ALTER TABLE "BaseItems_jf" RENAME TO "BaseItems";
CREATE INDEX "IX_BaseItems_ExtraType_OwnerId" ON "BaseItems"(
  "ExtraType",
  "OwnerId"
);
CREATE INDEX "IX_BaseItems_IsFolder_TopParentId_IsVirtualItem_PresentationUniqueKey_DateCreated" ON "BaseItems"(
  "IsFolder",
  "TopParentId",
  "IsVirtualItem",
  "PresentationUniqueKey",
  "DateCreated"
);
CREATE INDEX "IX_BaseItems_MediaType_TopParentId_IsVirtualItem_PresentationUniqueKey" ON "BaseItems"(
  "MediaType",
  "TopParentId",
  "IsVirtualItem",
  "PresentationUniqueKey"
);
CREATE INDEX "IX_BaseItems_Name" ON "BaseItems"("Name");
CREATE INDEX "IX_BaseItems_OwnerId" ON "BaseItems"("OwnerId");
CREATE INDEX "IX_BaseItems_ParentId" ON "BaseItems"("ParentId");
CREATE INDEX "IX_BaseItems_Path" ON "BaseItems"("Path");
CREATE INDEX "IX_BaseItems_PresentationUniqueKey" ON "BaseItems"(
  "PresentationUniqueKey"
);
CREATE INDEX "IX_BaseItems_PrimaryVersionId" ON "BaseItems"(
  "PrimaryVersionId"
) WHERE "PrimaryVersionId" IS NOT NULL;
CREATE INDEX "IX_BaseItems_SeasonId" ON "BaseItems"("SeasonId");
CREATE INDEX "IX_BaseItems_SeriesId" ON "BaseItems"("SeriesId");
CREATE INDEX "IX_BaseItems_SeriesName" ON "BaseItems"("SeriesName");
CREATE INDEX "IX_BaseItems_TopParentId_Id" ON "BaseItems"("TopParentId", "Id");
CREATE INDEX "IX_BaseItems_TopParentId_IsFolder_IsVirtualItem_DateCreated" ON "BaseItems"(
  "TopParentId",
  "IsFolder",
  "IsVirtualItem",
  "DateCreated"
);
CREATE INDEX "IX_BaseItems_TopParentId_MediaType_IsVirtualItem_DateCreated" ON "BaseItems"(
  "TopParentId",
  "MediaType",
  "IsVirtualItem",
  "DateCreated"
);
CREATE INDEX "IX_BaseItems_TopParentId_Type_IsVirtualItem" ON "BaseItems"(
  "TopParentId",
  "Type",
  "IsVirtualItem"
) WHERE "PrimaryVersionId" IS NULL 
    AND("OwnerId" IS NULL OR "ExtraType" IS NOT NULL);
CREATE INDEX "IX_BaseItems_TopParentId_Type_IsVirtualItem_DateCreated" ON "BaseItems"(
  "TopParentId",
  "Type",
  "IsVirtualItem",
  "DateCreated"
);
CREATE INDEX "IX_BaseItems_Type_CleanName" ON "BaseItems"("Type", "CleanName");
CREATE INDEX "IX_BaseItems_Type_SeriesPresentationUniqueKey_IsFolder_IsVirtualItem" ON "BaseItems"(
  "Type",
  "SeriesPresentationUniqueKey",
  "IsFolder",
  "IsVirtualItem"
);
CREATE INDEX "IX_BaseItems_Type_SeriesPresentationUniqueKey_ParentIndexNumber_IndexNumber" ON "BaseItems"(
  "Type",
  "SeriesPresentationUniqueKey",
  "ParentIndexNumber",
  "IndexNumber"
);
CREATE INDEX "IX_BaseItems_Type_SeriesPresentationUniqueKey_PresentationUniqueKey_SortName" ON "BaseItems"(
  "Type",
  "SeriesPresentationUniqueKey",
  "PresentationUniqueKey",
  "SortName"
);
CREATE INDEX "IX_BaseItems_Type_TopParentId_Id" ON "BaseItems"(
  "Type",
  "TopParentId",
  "Id"
);
CREATE INDEX "IX_BaseItems_Type_TopParentId_IsVirtualItem_PresentationUniqueKey_DateCreated" ON "BaseItems"(
  "Type",
  "TopParentId",
  "IsVirtualItem",
  "PresentationUniqueKey",
  "DateCreated"
);
CREATE INDEX "IX_BaseItems_Type_TopParentId_PresentationUniqueKey" ON "BaseItems"(
  "Type",
  "TopParentId",
  "PresentationUniqueKey"
);
CREATE INDEX "IX_BaseItems_Type_TopParentId_SortName" ON "BaseItems"(
  "Type",
  "TopParentId",
  "SortName"
);
CREATE INDEX "IX_BaseItems_Type_TopParentId_StartDate" ON "BaseItems"(
  "Type",
  "TopParentId",
  "StartDate"
);
-- Ferrofin's own BaseItems indexes: only the two shapes 12.0 does not ship.
-- The other fourteen `FerrofinIX_BaseItems_*` indexes of 0014/0018 are
-- column-for-column (and WHERE-for-WHERE) duplicates of the `IX_BaseItems_*`
-- set 12.0 added above; a duplicate index changes no plan and doubles the
-- write cost, so they are not recreated here.
CREATE INDEX "FerrofinIX_BaseItems_IsLocked"
ON "BaseItems"(
  "IsLocked"
)
WHERE "IsLocked" = 1;
CREATE INDEX "FerrofinIX_BaseItems_SortName_Name"
ON "BaseItems"(
  "SortName",
  "Name"
);

-- ── LinkedChildren: new in 12.0 ─────────────────────────────
CREATE TABLE "LinkedChildren"(
  "ParentId" TEXT NOT NULL,
  "SortOrder" INTEGER NOT NULL,
  "ChildId" TEXT NOT NULL,
  "ChildType" INTEGER NOT NULL,
  CONSTRAINT "PK_LinkedChildren" PRIMARY KEY("ParentId", "SortOrder"),
  CONSTRAINT "FK_LinkedChildren_BaseItems_ChildId" FOREIGN KEY("ChildId") REFERENCES "BaseItems"("Id"),
  CONSTRAINT "FK_LinkedChildren_BaseItems_ParentId" FOREIGN KEY("ParentId") REFERENCES "BaseItems"("Id")
);
CREATE INDEX "IX_LinkedChildren_ChildId_ChildType" ON "LinkedChildren"(
  "ChildId",
  "ChildType"
);
CREATE INDEX "IX_LinkedChildren_ParentId_ChildType" ON "LinkedChildren"(
  "ParentId",
  "ChildType"
);

-- ── index set on tables that keep their shape ─────────────────────────────
DROP INDEX "IX_BaseItemImageInfos_ItemId";
DROP INDEX "IX_BaseItemProviders_ProviderId_ProviderValue_ItemId";
DROP INDEX "IX_Devices_DeviceId";
DROP INDEX "IX_PeopleBaseItemMap_PeopleId";
DROP INDEX "IX_UserData_UserId";
CREATE INDEX "IX_BaseItemImageInfos_ItemId_ImageType" ON "BaseItemImageInfos"(
  "ItemId",
  "ImageType"
);
CREATE INDEX "IX_BaseItemProviders_ProviderId_ItemId_ProviderValue" ON "BaseItemProviders"(
  "ProviderId",
  "ItemId",
  "ProviderValue"
);
CREATE INDEX "IX_PeopleBaseItemMap_PeopleId_ItemId" ON "PeopleBaseItemMap"(
  "PeopleId",
  "ItemId"
);
CREATE INDEX "IX_Peoples_NameLower" ON "Peoples"(lower("Name"));
CREATE INDEX "IX_UserData_UserId_IsFavorite_ItemId" ON "UserData"(
  "UserId",
  "IsFavorite",
  "ItemId"
);
CREATE INDEX "IX_UserData_UserId_ItemId_LastPlayedDate" ON "UserData"(
  "UserId",
  "ItemId",
  "LastPlayedDate"
);
CREATE INDEX "IX_UserData_UserId_Played_ItemId" ON "UserData"(
  "UserId",
  "Played",
  "ItemId"
);
