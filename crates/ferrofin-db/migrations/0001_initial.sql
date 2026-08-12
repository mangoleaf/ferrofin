-- Hermit initial schema — head of Jellyfin's EF Core model snapshot
-- (JellyfinDbModelSnapshot.cs, ProductVersion 10.0.12).
--
-- Verbatim port of the 31 head tables: exact table/column names, SQLite
-- column types (INTEGER/TEXT/REAL/BLOB), primary keys, and every index
-- (including UNIQUE and filtered/partial indexes). EF filter predicates that
-- used bracket-quoted identifiers (`[UserId]`) are rewritten with SQLite
-- double-quotes. Foreign keys are declared inline where the snapshot's
-- relationship configuration defines them.

-- ── AccessSchedules ────────────────────────────────────────────────────────
CREATE TABLE "AccessSchedules" (
    "Id"        INTEGER NOT NULL,
    "DayOfWeek" INTEGER NOT NULL,
    "EndHour"   REAL    NOT NULL,
    "StartHour" REAL    NOT NULL,
    "UserId"    TEXT    NOT NULL,
    CONSTRAINT "PK_AccessSchedules" PRIMARY KEY ("Id"),
    CONSTRAINT "FK_AccessSchedules_Users_UserId" FOREIGN KEY ("UserId")
        REFERENCES "Users" ("Id") ON DELETE CASCADE
);
CREATE INDEX "IX_AccessSchedules_UserId" ON "AccessSchedules" ("UserId");

-- ── ActivityLogs ───────────────────────────────────────────────────────────
CREATE TABLE "ActivityLogs" (
    "Id"            INTEGER NOT NULL,
    "DateCreated"   TEXT    NOT NULL,
    "ItemId"        TEXT,
    "LogSeverity"   INTEGER NOT NULL,
    "Name"          TEXT    NOT NULL,
    "Overview"      TEXT,
    "RowVersion"    INTEGER NOT NULL,
    "ShortOverview" TEXT,
    "Type"          TEXT    NOT NULL,
    "UserId"        TEXT    NOT NULL,
    CONSTRAINT "PK_ActivityLogs" PRIMARY KEY ("Id")
);
CREATE INDEX "IX_ActivityLogs_DateCreated" ON "ActivityLogs" ("DateCreated");

-- ── AncestorIds ────────────────────────────────────────────────────────────
CREATE TABLE "AncestorIds" (
    "ItemId"       TEXT NOT NULL,
    "ParentItemId" TEXT NOT NULL,
    CONSTRAINT "PK_AncestorIds" PRIMARY KEY ("ItemId", "ParentItemId"),
    CONSTRAINT "FK_AncestorIds_BaseItems_ItemId" FOREIGN KEY ("ItemId")
        REFERENCES "BaseItems" ("Id") ON DELETE CASCADE,
    CONSTRAINT "FK_AncestorIds_BaseItems_ParentItemId" FOREIGN KEY ("ParentItemId")
        REFERENCES "BaseItems" ("Id") ON DELETE CASCADE
);
CREATE INDEX "IX_AncestorIds_ParentItemId" ON "AncestorIds" ("ParentItemId");

-- ── AttachmentStreamInfos ──────────────────────────────────────────────────
CREATE TABLE "AttachmentStreamInfos" (
    "ItemId"   TEXT    NOT NULL,
    "Index"    INTEGER NOT NULL,
    "Codec"    TEXT,
    "CodecTag" TEXT,
    "Comment"  TEXT,
    "Filename" TEXT,
    "MimeType" TEXT,
    CONSTRAINT "PK_AttachmentStreamInfos" PRIMARY KEY ("ItemId", "Index"),
    CONSTRAINT "FK_AttachmentStreamInfos_BaseItems_ItemId" FOREIGN KEY ("ItemId")
        REFERENCES "BaseItems" ("Id") ON DELETE CASCADE
);

-- ── BaseItems ──────────────────────────────────────────────────────────────
CREATE TABLE "BaseItems" (
    "Id"                              TEXT    NOT NULL,
    "Album"                           TEXT,
    "AlbumArtists"                    TEXT,
    "Artists"                         TEXT,
    "Audio"                           INTEGER,
    "ChannelId"                       TEXT,
    "CleanName"                       TEXT,
    "CommunityRating"                 REAL,
    "CriticRating"                    REAL,
    "CustomRating"                    TEXT,
    "Data"                            TEXT,
    "DateCreated"                     TEXT,
    "DateLastMediaAdded"              TEXT,
    "DateLastRefreshed"               TEXT,
    "DateLastSaved"                   TEXT,
    "DateModified"                    TEXT,
    "EndDate"                         TEXT,
    "EpisodeTitle"                    TEXT,
    "ExternalId"                      TEXT,
    "ExternalSeriesId"                TEXT,
    "ExternalServiceId"               TEXT,
    "ExtraType"                       INTEGER,
    "ForcedSortName"                  TEXT,
    "Genres"                          TEXT,
    "Height"                          INTEGER,
    "IndexNumber"                     INTEGER,
    "InheritedParentalRatingSubValue" INTEGER,
    "InheritedParentalRatingValue"    INTEGER,
    "IsFolder"                        INTEGER NOT NULL,
    "IsInMixedFolder"                 INTEGER NOT NULL,
    "IsLocked"                        INTEGER NOT NULL,
    "IsMovie"                         INTEGER NOT NULL,
    "IsRepeat"                        INTEGER NOT NULL,
    "IsSeries"                        INTEGER NOT NULL,
    "IsVirtualItem"                   INTEGER NOT NULL,
    "LUFS"                            REAL,
    "MediaType"                       TEXT,
    "Name"                            TEXT,
    "NormalizationGain"               REAL,
    "OfficialRating"                  TEXT,
    "OriginalLanguage"                TEXT,
    "OriginalTitle"                   TEXT,
    "Overview"                        TEXT,
    "OwnerId"                         TEXT,
    "ParentId"                        TEXT,
    "ParentIndexNumber"               INTEGER,
    "Path"                            TEXT,
    "PreferredMetadataCountryCode"    TEXT,
    "PreferredMetadataLanguage"       TEXT,
    "PremiereDate"                    TEXT,
    "PresentationUniqueKey"           TEXT,
    "PrimaryVersionId"                TEXT,
    "ProductionLocations"             TEXT,
    "ProductionYear"                  INTEGER,
    "RunTimeTicks"                    INTEGER,
    "SeasonId"                        TEXT,
    "SeasonName"                      TEXT,
    "SeriesId"                        TEXT,
    "SeriesName"                      TEXT,
    "SeriesPresentationUniqueKey"     TEXT,
    "ShowId"                          TEXT,
    "Size"                            INTEGER,
    "SortName"                        TEXT,
    "StartDate"                       TEXT,
    "Studios"                         TEXT,
    "Tagline"                         TEXT,
    "Tags"                            TEXT,
    "TopParentId"                     TEXT,
    "TotalBitrate"                    INTEGER,
    "Type"                            TEXT    NOT NULL,
    "UnratedType"                     TEXT,
    "Width"                           INTEGER,
    CONSTRAINT "PK_BaseItems" PRIMARY KEY ("Id"),
    CONSTRAINT "FK_BaseItems_BaseItems_OwnerId" FOREIGN KEY ("OwnerId")
        REFERENCES "BaseItems" ("Id"),
    CONSTRAINT "FK_BaseItems_BaseItems_ParentId" FOREIGN KEY ("ParentId")
        REFERENCES "BaseItems" ("Id") ON DELETE CASCADE
);
CREATE INDEX "IX_BaseItems_Name"                  ON "BaseItems" ("Name");
CREATE INDEX "IX_BaseItems_OwnerId"               ON "BaseItems" ("OwnerId");
CREATE INDEX "IX_BaseItems_ParentId"              ON "BaseItems" ("ParentId");
CREATE INDEX "IX_BaseItems_Path"                  ON "BaseItems" ("Path");
CREATE INDEX "IX_BaseItems_PresentationUniqueKey" ON "BaseItems" ("PresentationUniqueKey");
CREATE INDEX "IX_BaseItems_SeasonId"              ON "BaseItems" ("SeasonId");
CREATE INDEX "IX_BaseItems_SeriesId"              ON "BaseItems" ("SeriesId");
CREATE INDEX "IX_BaseItems_SeriesName"            ON "BaseItems" ("SeriesName");
CREATE INDEX "IX_BaseItems_ExtraType_OwnerId"     ON "BaseItems" ("ExtraType", "OwnerId");
CREATE INDEX "IX_BaseItems_TopParentId_Id"        ON "BaseItems" ("TopParentId", "Id");
CREATE INDEX "IX_BaseItems_Type_CleanName"        ON "BaseItems" ("Type", "CleanName");
CREATE INDEX "IX_BaseItems_TopParentId_Type_IsVirtualItem"
    ON "BaseItems" ("TopParentId", "Type", "IsVirtualItem")
    WHERE "PrimaryVersionId" IS NULL AND ("OwnerId" IS NULL OR "ExtraType" IS NOT NULL);
CREATE INDEX "IX_BaseItems_Type_TopParentId_Id"
    ON "BaseItems" ("Type", "TopParentId", "Id");
CREATE INDEX "IX_BaseItems_Type_TopParentId_PresentationUniqueKey"
    ON "BaseItems" ("Type", "TopParentId", "PresentationUniqueKey");
CREATE INDEX "IX_BaseItems_Type_TopParentId_SortName"
    ON "BaseItems" ("Type", "TopParentId", "SortName");
CREATE INDEX "IX_BaseItems_Type_TopParentId_StartDate"
    ON "BaseItems" ("Type", "TopParentId", "StartDate");
CREATE INDEX "IX_BaseItems_MediaType_TopParentId_IsVirtualItem_PresentationUniqueKey"
    ON "BaseItems" ("MediaType", "TopParentId", "IsVirtualItem", "PresentationUniqueKey");
CREATE INDEX "IX_BaseItems_TopParentId_IsFolder_IsVirtualItem_DateCreated"
    ON "BaseItems" ("TopParentId", "IsFolder", "IsVirtualItem", "DateCreated");
CREATE INDEX "IX_BaseItems_TopParentId_MediaType_IsVirtualItem_DateCreated"
    ON "BaseItems" ("TopParentId", "MediaType", "IsVirtualItem", "DateCreated");
CREATE INDEX "IX_BaseItems_TopParentId_Type_IsVirtualItem_DateCreated"
    ON "BaseItems" ("TopParentId", "Type", "IsVirtualItem", "DateCreated");
CREATE INDEX "IX_BaseItems_Type_SeriesPresentationUniqueKey_IsFolder_IsVirtualItem"
    ON "BaseItems" ("Type", "SeriesPresentationUniqueKey", "IsFolder", "IsVirtualItem");
CREATE INDEX "IX_BaseItems_Type_SeriesPresentationUniqueKey_ParentIndexNumber_IndexNumber"
    ON "BaseItems" ("Type", "SeriesPresentationUniqueKey", "ParentIndexNumber", "IndexNumber");
CREATE INDEX "IX_BaseItems_Type_SeriesPresentationUniqueKey_PresentationUniqueKey_SortName"
    ON "BaseItems" ("Type", "SeriesPresentationUniqueKey", "PresentationUniqueKey", "SortName");
CREATE INDEX "IX_BaseItems_IsFolder_TopParentId_IsVirtualItem_PresentationUniqueKey_DateCreated"
    ON "BaseItems" ("IsFolder", "TopParentId", "IsVirtualItem", "PresentationUniqueKey", "DateCreated");
CREATE INDEX "IX_BaseItems_Type_TopParentId_IsVirtualItem_PresentationUniqueKey_DateCreated"
    ON "BaseItems" ("Type", "TopParentId", "IsVirtualItem", "PresentationUniqueKey", "DateCreated");

-- Placeholder item for UserData detached from its original item.
INSERT INTO "BaseItems"
    ("Id", "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie",
     "IsRepeat", "IsSeries", "IsVirtualItem", "Name", "Type")
VALUES
    ('00000000-0000-0000-0000-000000000001', 0, 0, 0, 0, 0, 0, 0,
     'This is a placeholder item for UserData that has been detached from its original item',
     'PLACEHOLDER');

-- ── BaseItemImageInfos ─────────────────────────────────────────────────────
CREATE TABLE "BaseItemImageInfos" (
    "Id"           TEXT    NOT NULL,
    "Blurhash"     BLOB,
    "DateModified" TEXT,
    "Height"       INTEGER NOT NULL,
    "ImageType"    INTEGER NOT NULL,
    "ItemId"       TEXT    NOT NULL,
    "Path"         TEXT    NOT NULL,
    "Width"        INTEGER NOT NULL,
    CONSTRAINT "PK_BaseItemImageInfos" PRIMARY KEY ("Id"),
    CONSTRAINT "FK_BaseItemImageInfos_BaseItems_ItemId" FOREIGN KEY ("ItemId")
        REFERENCES "BaseItems" ("Id") ON DELETE CASCADE
);
CREATE INDEX "IX_BaseItemImageInfos_ItemId_ImageType"
    ON "BaseItemImageInfos" ("ItemId", "ImageType");

-- ── BaseItemMetadataFields ─────────────────────────────────────────────────
CREATE TABLE "BaseItemMetadataFields" (
    "Id"     INTEGER NOT NULL,
    "ItemId" TEXT    NOT NULL,
    CONSTRAINT "PK_BaseItemMetadataFields" PRIMARY KEY ("Id", "ItemId"),
    CONSTRAINT "FK_BaseItemMetadataFields_BaseItems_ItemId" FOREIGN KEY ("ItemId")
        REFERENCES "BaseItems" ("Id") ON DELETE CASCADE
);
CREATE INDEX "IX_BaseItemMetadataFields_ItemId"
    ON "BaseItemMetadataFields" ("ItemId");

-- ── BaseItemProviders ──────────────────────────────────────────────────────
CREATE TABLE "BaseItemProviders" (
    "ItemId"        TEXT NOT NULL,
    "ProviderId"    TEXT NOT NULL,
    "ProviderValue" TEXT NOT NULL,
    CONSTRAINT "PK_BaseItemProviders" PRIMARY KEY ("ItemId", "ProviderId"),
    CONSTRAINT "FK_BaseItemProviders_BaseItems_ItemId" FOREIGN KEY ("ItemId")
        REFERENCES "BaseItems" ("Id") ON DELETE CASCADE
);
CREATE INDEX "IX_BaseItemProviders_ProviderId_ItemId_ProviderValue"
    ON "BaseItemProviders" ("ProviderId", "ItemId", "ProviderValue");

-- ── BaseItemTrailerTypes ───────────────────────────────────────────────────
CREATE TABLE "BaseItemTrailerTypes" (
    "Id"     INTEGER NOT NULL,
    "ItemId" TEXT    NOT NULL,
    CONSTRAINT "PK_BaseItemTrailerTypes" PRIMARY KEY ("Id", "ItemId"),
    CONSTRAINT "FK_BaseItemTrailerTypes_BaseItems_ItemId" FOREIGN KEY ("ItemId")
        REFERENCES "BaseItems" ("Id") ON DELETE CASCADE
);
CREATE INDEX "IX_BaseItemTrailerTypes_ItemId"
    ON "BaseItemTrailerTypes" ("ItemId");

-- ── Chapters ───────────────────────────────────────────────────────────────
CREATE TABLE "Chapters" (
    "ItemId"             TEXT    NOT NULL,
    "ChapterIndex"       INTEGER NOT NULL,
    "ImageDateModified"  TEXT,
    "ImagePath"          TEXT,
    "Name"               TEXT,
    "StartPositionTicks" INTEGER NOT NULL,
    CONSTRAINT "PK_Chapters" PRIMARY KEY ("ItemId", "ChapterIndex"),
    CONSTRAINT "FK_Chapters_BaseItems_ItemId" FOREIGN KEY ("ItemId")
        REFERENCES "BaseItems" ("Id") ON DELETE CASCADE
);

-- ── CustomItemDisplayPreferences ───────────────────────────────────────────
CREATE TABLE "CustomItemDisplayPreferences" (
    "Id"     INTEGER NOT NULL,
    "Client" TEXT    NOT NULL,
    "ItemId" TEXT    NOT NULL,
    "Key"    TEXT    NOT NULL,
    "UserId" TEXT    NOT NULL,
    "Value"  TEXT,
    CONSTRAINT "PK_CustomItemDisplayPreferences" PRIMARY KEY ("Id")
);
CREATE UNIQUE INDEX "IX_CustomItemDisplayPreferences_UserId_ItemId_Client_Key"
    ON "CustomItemDisplayPreferences" ("UserId", "ItemId", "Client", "Key");

-- ── DisplayPreferences ─────────────────────────────────────────────────────
CREATE TABLE "DisplayPreferences" (
    "Id"                        INTEGER NOT NULL,
    "ChromecastVersion"         INTEGER NOT NULL,
    "Client"                    TEXT    NOT NULL,
    "DashboardTheme"            TEXT,
    "EnableNextVideoInfoOverlay" INTEGER NOT NULL,
    "IndexBy"                   INTEGER,
    "ItemId"                    TEXT    NOT NULL,
    "ScrollDirection"           INTEGER NOT NULL,
    "ShowBackdrop"              INTEGER NOT NULL,
    "ShowSidebar"               INTEGER NOT NULL,
    "SkipBackwardLength"        INTEGER NOT NULL,
    "SkipForwardLength"         INTEGER NOT NULL,
    "TvHome"                    TEXT,
    "UserId"                    TEXT    NOT NULL,
    CONSTRAINT "PK_DisplayPreferences" PRIMARY KEY ("Id"),
    CONSTRAINT "FK_DisplayPreferences_Users_UserId" FOREIGN KEY ("UserId")
        REFERENCES "Users" ("Id") ON DELETE CASCADE
);
CREATE UNIQUE INDEX "IX_DisplayPreferences_UserId_ItemId_Client"
    ON "DisplayPreferences" ("UserId", "ItemId", "Client");

-- ── HomeSection ────────────────────────────────────────────────────────────
CREATE TABLE "HomeSection" (
    "Id"                    INTEGER NOT NULL,
    "DisplayPreferencesId"  INTEGER NOT NULL,
    "Order"                 INTEGER NOT NULL,
    "Type"                  INTEGER NOT NULL,
    CONSTRAINT "PK_HomeSection" PRIMARY KEY ("Id"),
    CONSTRAINT "FK_HomeSection_DisplayPreferences_DisplayPreferencesId"
        FOREIGN KEY ("DisplayPreferencesId")
        REFERENCES "DisplayPreferences" ("Id") ON DELETE CASCADE
);
CREATE INDEX "IX_HomeSection_DisplayPreferencesId"
    ON "HomeSection" ("DisplayPreferencesId");

-- ── ImageInfos ─────────────────────────────────────────────────────────────
CREATE TABLE "ImageInfos" (
    "Id"           INTEGER NOT NULL,
    "LastModified" TEXT    NOT NULL,
    "Path"         TEXT    NOT NULL,
    "UserId"       TEXT,
    CONSTRAINT "PK_ImageInfos" PRIMARY KEY ("Id"),
    CONSTRAINT "FK_ImageInfos_Users_UserId" FOREIGN KEY ("UserId")
        REFERENCES "Users" ("Id") ON DELETE CASCADE
);
CREATE UNIQUE INDEX "IX_ImageInfos_UserId" ON "ImageInfos" ("UserId");

-- ── ItemDisplayPreferences ─────────────────────────────────────────────────
CREATE TABLE "ItemDisplayPreferences" (
    "Id"              INTEGER NOT NULL,
    "Client"          TEXT    NOT NULL,
    "IndexBy"         INTEGER,
    "ItemId"          TEXT    NOT NULL,
    "RememberIndexing" INTEGER NOT NULL,
    "RememberSorting" INTEGER NOT NULL,
    "SortBy"          TEXT    NOT NULL,
    "SortOrder"       INTEGER NOT NULL,
    "UserId"          TEXT    NOT NULL,
    "ViewType"        INTEGER NOT NULL,
    CONSTRAINT "PK_ItemDisplayPreferences" PRIMARY KEY ("Id"),
    CONSTRAINT "FK_ItemDisplayPreferences_Users_UserId" FOREIGN KEY ("UserId")
        REFERENCES "Users" ("Id") ON DELETE CASCADE
);
CREATE INDEX "IX_ItemDisplayPreferences_UserId"
    ON "ItemDisplayPreferences" ("UserId");

-- ── ItemValues ─────────────────────────────────────────────────────────────
CREATE TABLE "ItemValues" (
    "ItemValueId" TEXT    NOT NULL,
    "CleanValue"  TEXT    NOT NULL,
    "Type"        INTEGER NOT NULL,
    "Value"       TEXT    NOT NULL,
    CONSTRAINT "PK_ItemValues" PRIMARY KEY ("ItemValueId")
);
CREATE INDEX "IX_ItemValues_Type_CleanValue"
    ON "ItemValues" ("Type", "CleanValue");
CREATE UNIQUE INDEX "IX_ItemValues_Type_Value"
    ON "ItemValues" ("Type", "Value");

-- ── ItemValuesMap ──────────────────────────────────────────────────────────
CREATE TABLE "ItemValuesMap" (
    "ItemValueId" TEXT NOT NULL,
    "ItemId"      TEXT NOT NULL,
    CONSTRAINT "PK_ItemValuesMap" PRIMARY KEY ("ItemValueId", "ItemId"),
    CONSTRAINT "FK_ItemValuesMap_BaseItems_ItemId" FOREIGN KEY ("ItemId")
        REFERENCES "BaseItems" ("Id") ON DELETE CASCADE,
    CONSTRAINT "FK_ItemValuesMap_ItemValues_ItemValueId" FOREIGN KEY ("ItemValueId")
        REFERENCES "ItemValues" ("ItemValueId") ON DELETE CASCADE
);
CREATE INDEX "IX_ItemValuesMap_ItemId" ON "ItemValuesMap" ("ItemId");

-- ── KeyframeData ───────────────────────────────────────────────────────────
CREATE TABLE "KeyframeData" (
    "ItemId"        TEXT    NOT NULL,
    "KeyframeTicks" TEXT,
    "TotalDuration" INTEGER NOT NULL,
    CONSTRAINT "PK_KeyframeData" PRIMARY KEY ("ItemId"),
    CONSTRAINT "FK_KeyframeData_BaseItems_ItemId" FOREIGN KEY ("ItemId")
        REFERENCES "BaseItems" ("Id") ON DELETE CASCADE
);

-- ── LinkedChildren ─────────────────────────────────────────────────────────
CREATE TABLE "LinkedChildren" (
    "ParentId"  TEXT    NOT NULL,
    "ChildId"   TEXT    NOT NULL,
    "ChildType" INTEGER NOT NULL,
    "SortOrder" INTEGER,
    CONSTRAINT "PK_LinkedChildren" PRIMARY KEY ("ParentId", "ChildId"),
    CONSTRAINT "FK_LinkedChildren_BaseItems_ChildId" FOREIGN KEY ("ChildId")
        REFERENCES "BaseItems" ("Id"),
    CONSTRAINT "FK_LinkedChildren_BaseItems_ParentId" FOREIGN KEY ("ParentId")
        REFERENCES "BaseItems" ("Id")
);
CREATE INDEX "IX_LinkedChildren_ChildId_ChildType"
    ON "LinkedChildren" ("ChildId", "ChildType");
CREATE INDEX "IX_LinkedChildren_ParentId_ChildType"
    ON "LinkedChildren" ("ParentId", "ChildType");
CREATE INDEX "IX_LinkedChildren_ParentId_SortOrder"
    ON "LinkedChildren" ("ParentId", "SortOrder");

-- ── MediaSegments ──────────────────────────────────────────────────────────
CREATE TABLE "MediaSegments" (
    "Id"                TEXT    NOT NULL,
    "EndTicks"          INTEGER NOT NULL,
    "ItemId"            TEXT    NOT NULL,
    "SegmentProviderId" TEXT    NOT NULL,
    "StartTicks"        INTEGER NOT NULL,
    "Type"              INTEGER NOT NULL,
    CONSTRAINT "PK_MediaSegments" PRIMARY KEY ("Id")
);

-- ── MediaStreamInfos ───────────────────────────────────────────────────────
CREATE TABLE "MediaStreamInfos" (
    "ItemId"                    TEXT    NOT NULL,
    "StreamIndex"               INTEGER NOT NULL,
    "AspectRatio"               TEXT,
    "AverageFrameRate"          REAL,
    "BitDepth"                  INTEGER,
    "BitRate"                   INTEGER,
    "BlPresentFlag"             INTEGER,
    "ChannelLayout"             TEXT,
    "Channels"                  INTEGER,
    "Codec"                     TEXT,
    "CodecTag"                  TEXT,
    "CodecTimeBase"             TEXT,
    "ColorPrimaries"            TEXT,
    "ColorSpace"                TEXT,
    "ColorTransfer"             TEXT,
    "Comment"                   TEXT,
    "DvBlSignalCompatibilityId" INTEGER,
    "DvLevel"                   INTEGER,
    "DvProfile"                 INTEGER,
    "DvVersionMajor"            INTEGER,
    "DvVersionMinor"            INTEGER,
    "ElPresentFlag"             INTEGER,
    "Hdr10PlusPresentFlag"      INTEGER,
    "Height"                    INTEGER,
    "IsAnamorphic"              INTEGER,
    "IsAvc"                     INTEGER,
    "IsDefault"                 INTEGER NOT NULL,
    "IsExternal"                INTEGER NOT NULL,
    "IsForced"                  INTEGER NOT NULL,
    "IsHearingImpaired"         INTEGER,
    "IsInterlaced"              INTEGER,
    "IsOriginal"                INTEGER NOT NULL,
    "KeyFrames"                 TEXT,
    "Language"                  TEXT,
    "Level"                     REAL,
    "NalLengthSize"             TEXT,
    "Path"                      TEXT,
    "PixelFormat"               TEXT,
    "Profile"                   TEXT,
    "RealFrameRate"             REAL,
    "RefFrames"                 INTEGER,
    "Rotation"                  INTEGER,
    "RpuPresentFlag"            INTEGER,
    "SampleRate"                INTEGER,
    "StreamType"                INTEGER NOT NULL,
    "TimeBase"                  TEXT,
    "Title"                     TEXT,
    "Width"                     INTEGER,
    CONSTRAINT "PK_MediaStreamInfos" PRIMARY KEY ("ItemId", "StreamIndex"),
    CONSTRAINT "FK_MediaStreamInfos_BaseItems_ItemId" FOREIGN KEY ("ItemId")
        REFERENCES "BaseItems" ("Id") ON DELETE CASCADE
);

-- ── Peoples ────────────────────────────────────────────────────────────────
CREATE TABLE "Peoples" (
    "Id"         TEXT NOT NULL,
    "Name"       TEXT NOT NULL,
    "PersonType" TEXT,
    CONSTRAINT "PK_Peoples" PRIMARY KEY ("Id")
);
CREATE INDEX "IX_Peoples_Name" ON "Peoples" ("Name");

-- ── PeopleBaseItemMap ──────────────────────────────────────────────────────
CREATE TABLE "PeopleBaseItemMap" (
    "ItemId"    TEXT    NOT NULL,
    "PeopleId"  TEXT    NOT NULL,
    "Role"      TEXT    NOT NULL,
    "ListOrder" INTEGER,
    "SortOrder" INTEGER,
    CONSTRAINT "PK_PeopleBaseItemMap" PRIMARY KEY ("ItemId", "PeopleId", "Role"),
    CONSTRAINT "FK_PeopleBaseItemMap_BaseItems_ItemId" FOREIGN KEY ("ItemId")
        REFERENCES "BaseItems" ("Id") ON DELETE CASCADE,
    CONSTRAINT "FK_PeopleBaseItemMap_Peoples_PeopleId" FOREIGN KEY ("PeopleId")
        REFERENCES "Peoples" ("Id") ON DELETE CASCADE
);
CREATE INDEX "IX_PeopleBaseItemMap_PeopleId"
    ON "PeopleBaseItemMap" ("PeopleId");
CREATE INDEX "IX_PeopleBaseItemMap_ItemId_ListOrder"
    ON "PeopleBaseItemMap" ("ItemId", "ListOrder");
CREATE INDEX "IX_PeopleBaseItemMap_ItemId_SortOrder"
    ON "PeopleBaseItemMap" ("ItemId", "SortOrder");

-- ── Permissions ────────────────────────────────────────────────────────────
CREATE TABLE "Permissions" (
    "Id"                         INTEGER NOT NULL,
    "Kind"                       INTEGER NOT NULL,
    "Permission_Permissions_Guid" TEXT,
    "RowVersion"                 INTEGER NOT NULL,
    "UserId"                     TEXT,
    "Value"                      INTEGER NOT NULL,
    CONSTRAINT "PK_Permissions" PRIMARY KEY ("Id"),
    CONSTRAINT "FK_Permissions_Users_UserId" FOREIGN KEY ("UserId")
        REFERENCES "Users" ("Id") ON DELETE CASCADE
);
CREATE UNIQUE INDEX "IX_Permissions_UserId_Kind"
    ON "Permissions" ("UserId", "Kind") WHERE "UserId" IS NOT NULL;

-- ── Preferences ────────────────────────────────────────────────────────────
CREATE TABLE "Preferences" (
    "Id"                          INTEGER NOT NULL,
    "Kind"                        INTEGER NOT NULL,
    "Preference_Preferences_Guid" TEXT,
    "RowVersion"                  INTEGER NOT NULL,
    "UserId"                      TEXT,
    "Value"                       TEXT    NOT NULL,
    CONSTRAINT "PK_Preferences" PRIMARY KEY ("Id"),
    CONSTRAINT "FK_Preferences_Users_UserId" FOREIGN KEY ("UserId")
        REFERENCES "Users" ("Id") ON DELETE CASCADE
);
CREATE UNIQUE INDEX "IX_Preferences_UserId_Kind"
    ON "Preferences" ("UserId", "Kind") WHERE "UserId" IS NOT NULL;

-- ── ApiKeys ────────────────────────────────────────────────────────────────
CREATE TABLE "ApiKeys" (
    "Id"               INTEGER NOT NULL,
    "AccessToken"      TEXT    NOT NULL,
    "DateCreated"      TEXT    NOT NULL,
    "DateLastActivity" TEXT    NOT NULL,
    "Name"             TEXT    NOT NULL,
    CONSTRAINT "PK_ApiKeys" PRIMARY KEY ("Id")
);
CREATE UNIQUE INDEX "IX_ApiKeys_AccessToken" ON "ApiKeys" ("AccessToken");

-- ── Devices ────────────────────────────────────────────────────────────────
CREATE TABLE "Devices" (
    "Id"               INTEGER NOT NULL,
    "AccessToken"      TEXT    NOT NULL,
    "AppName"          TEXT    NOT NULL,
    "AppVersion"       TEXT    NOT NULL,
    "DateCreated"      TEXT    NOT NULL,
    "DateLastActivity" TEXT    NOT NULL,
    "DateModified"     TEXT    NOT NULL,
    "DeviceId"         TEXT    NOT NULL,
    "DeviceName"       TEXT    NOT NULL,
    "IsActive"         INTEGER NOT NULL,
    "UserId"           TEXT    NOT NULL,
    CONSTRAINT "PK_Devices" PRIMARY KEY ("Id"),
    CONSTRAINT "FK_Devices_Users_UserId" FOREIGN KEY ("UserId")
        REFERENCES "Users" ("Id") ON DELETE CASCADE
);
CREATE INDEX "IX_Devices_AccessToken_DateLastActivity"
    ON "Devices" ("AccessToken", "DateLastActivity");
CREATE INDEX "IX_Devices_DeviceId_DateLastActivity"
    ON "Devices" ("DeviceId", "DateLastActivity");
CREATE INDEX "IX_Devices_UserId_DeviceId"
    ON "Devices" ("UserId", "DeviceId");

-- ── DeviceOptions ──────────────────────────────────────────────────────────
CREATE TABLE "DeviceOptions" (
    "Id"         INTEGER NOT NULL,
    "CustomName" TEXT,
    "DeviceId"   TEXT    NOT NULL,
    CONSTRAINT "PK_DeviceOptions" PRIMARY KEY ("Id")
);
CREATE UNIQUE INDEX "IX_DeviceOptions_DeviceId" ON "DeviceOptions" ("DeviceId");

-- ── TrickplayInfos ─────────────────────────────────────────────────────────
CREATE TABLE "TrickplayInfos" (
    "ItemId"         TEXT    NOT NULL,
    "Width"          INTEGER NOT NULL,
    "Bandwidth"      INTEGER NOT NULL,
    "Height"         INTEGER NOT NULL,
    "Interval"       INTEGER NOT NULL,
    "ThumbnailCount" INTEGER NOT NULL,
    "TileHeight"     INTEGER NOT NULL,
    "TileWidth"      INTEGER NOT NULL,
    CONSTRAINT "PK_TrickplayInfos" PRIMARY KEY ("ItemId", "Width"),
    CONSTRAINT "FK_TrickplayInfos_BaseItems_ItemId" FOREIGN KEY ("ItemId")
        REFERENCES "BaseItems" ("Id") ON DELETE CASCADE
);

-- ── Users ──────────────────────────────────────────────────────────────────
CREATE TABLE "Users" (
    "Id"                          TEXT    NOT NULL,
    "AudioLanguagePreference"     TEXT,
    "AuthenticationProviderId"    TEXT    NOT NULL,
    "CastReceiverId"              TEXT,
    "DisplayCollectionsView"      INTEGER NOT NULL,
    "DisplayMissingEpisodes"      INTEGER NOT NULL,
    "EnableAutoLogin"             INTEGER NOT NULL,
    "EnableLocalPassword"         INTEGER NOT NULL,
    "EnableNextEpisodeAutoPlay"   INTEGER NOT NULL,
    "EnableUserPreferenceAccess"  INTEGER NOT NULL,
    "HidePlayedInLatest"          INTEGER NOT NULL,
    "InternalId"                  INTEGER NOT NULL,
    "InvalidLoginAttemptCount"    INTEGER NOT NULL,
    "LastActivityDate"            TEXT,
    "LastLoginDate"               TEXT,
    "LoginAttemptsBeforeLockout"  INTEGER,
    "MaxActiveSessions"           INTEGER NOT NULL,
    "MaxParentalRatingScore"      INTEGER,
    "MaxParentalRatingSubScore"   INTEGER,
    "MustUpdatePassword"          INTEGER NOT NULL,
    "NormalizedUsername"          TEXT    NOT NULL,
    "Password"                    TEXT,
    "PasswordResetProviderId"     TEXT    NOT NULL,
    "PlayDefaultAudioTrack"       INTEGER NOT NULL,
    "RememberAudioSelections"     INTEGER NOT NULL,
    "RememberSubtitleSelections"  INTEGER NOT NULL,
    "RemoteClientBitrateLimit"    INTEGER,
    "RowVersion"                  INTEGER NOT NULL,
    "SubtitleLanguagePreference"  TEXT,
    "SubtitleMode"                INTEGER NOT NULL,
    "SyncPlayAccess"              INTEGER NOT NULL,
    "Username"                    TEXT    NOT NULL,
    CONSTRAINT "PK_Users" PRIMARY KEY ("Id")
);
CREATE UNIQUE INDEX "IX_Users_NormalizedUsername" ON "Users" ("NormalizedUsername");
CREATE UNIQUE INDEX "IX_Users_Username"           ON "Users" ("Username");

-- ── UserData ───────────────────────────────────────────────────────────────
CREATE TABLE "UserData" (
    "ItemId"                TEXT    NOT NULL,
    "UserId"                TEXT    NOT NULL,
    "CustomDataKey"         TEXT    NOT NULL,
    "AudioStreamIndex"      INTEGER,
    "IsFavorite"            INTEGER NOT NULL,
    "LastPlayedDate"        TEXT,
    "Likes"                 INTEGER,
    "PlayCount"             INTEGER NOT NULL,
    "PlaybackPositionTicks" INTEGER NOT NULL,
    "Played"                INTEGER NOT NULL,
    "Rating"                REAL,
    "RetentionDate"         TEXT,
    "SubtitleStreamIndex"   INTEGER,
    CONSTRAINT "PK_UserData" PRIMARY KEY ("ItemId", "UserId", "CustomDataKey"),
    CONSTRAINT "FK_UserData_BaseItems_ItemId" FOREIGN KEY ("ItemId")
        REFERENCES "BaseItems" ("Id") ON DELETE CASCADE,
    CONSTRAINT "FK_UserData_Users_UserId" FOREIGN KEY ("UserId")
        REFERENCES "Users" ("Id") ON DELETE CASCADE
);
CREATE INDEX "IX_UserData_ItemId_UserId_IsFavorite"
    ON "UserData" ("ItemId", "UserId", "IsFavorite");
CREATE INDEX "IX_UserData_ItemId_UserId_LastPlayedDate"
    ON "UserData" ("ItemId", "UserId", "LastPlayedDate");
CREATE INDEX "IX_UserData_ItemId_UserId_PlaybackPositionTicks"
    ON "UserData" ("ItemId", "UserId", "PlaybackPositionTicks");
CREATE INDEX "IX_UserData_ItemId_UserId_Played"
    ON "UserData" ("ItemId", "UserId", "Played");
CREATE INDEX "IX_UserData_UserId_IsFavorite_ItemId"
    ON "UserData" ("UserId", "IsFavorite", "ItemId");
CREATE INDEX "IX_UserData_UserId_ItemId_LastPlayedDate"
    ON "UserData" ("UserId", "ItemId", "LastPlayedDate");
CREATE INDEX "IX_UserData_UserId_Played_ItemId"
    ON "UserData" ("UserId", "Played", "ItemId");
