-- 0009: Hermit-additive schema objects, adoption-safe.
--
-- An adopted Jellyfin database baselines migrations 0001-0007 without
-- running them (the Jellyfin file already has that shape), so every
-- Hermit-invented table and Hermit-only index must ALSO exist in an
-- additive migration that runs on both paths. IF NOT EXISTS makes this
-- a no-op on Hermit-native databases; on adopted databases it creates
-- the Hermit* namespace objects for the first time. DDL is the exact
-- post-0007 shape (byte-derived, not hand-written).

CREATE TABLE IF NOT EXISTS "HermitLinkedChildren" (
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
CREATE INDEX IF NOT EXISTS "HermitIX_LinkedChildren_ChildId_ChildType" ON "HermitLinkedChildren" ("ChildId", "ChildType");
CREATE INDEX IF NOT EXISTS "HermitIX_LinkedChildren_ParentId_ChildType" ON "HermitLinkedChildren" ("ParentId", "ChildType");
CREATE INDEX IF NOT EXISTS "HermitIX_LinkedChildren_ParentId_SortOrder" ON "HermitLinkedChildren" ("ParentId", "SortOrder");

CREATE TABLE IF NOT EXISTS "HermitLiveTvChannels" (
    "Id"          TEXT    NOT NULL,
    "TunerHostId" TEXT    NOT NULL,
    "TvgId"       TEXT    NOT NULL DEFAULT '',
    "Name"        TEXT    NOT NULL,
    "Number"      TEXT,
    "ImageUrl"    TEXT,
    "ChannelType" TEXT    NOT NULL DEFAULT 'Tv',
    "StreamUrl"   TEXT    NOT NULL,
    "SortIndex"   INTEGER NOT NULL DEFAULT 0,
    CONSTRAINT "PK_LiveTvChannels" PRIMARY KEY ("Id"),
    CONSTRAINT "FK_LiveTvChannels_TunerHosts_TunerHostId" FOREIGN KEY ("TunerHostId")
        REFERENCES "LiveTvTunerHosts" ("Id") ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS "HermitIX_LiveTvChannels_TunerHostId" ON "HermitLiveTvChannels" ("TunerHostId");
CREATE INDEX IF NOT EXISTS "HermitIX_LiveTvChannels_TvgId" ON "HermitLiveTvChannels" ("TvgId");

CREATE TABLE IF NOT EXISTS "HermitLiveTvListingProviders" (
    "Id"   TEXT NOT NULL,
    "Type" TEXT NOT NULL,
    "Path" TEXT NOT NULL DEFAULT '',
    "Data" TEXT NOT NULL,
    CONSTRAINT "PK_LiveTvListingProviders" PRIMARY KEY ("Id")
);

CREATE TABLE IF NOT EXISTS "HermitLiveTvPrograms" (
    "Id"             TEXT    NOT NULL,
    "ChannelId"      TEXT    NOT NULL,
    "StartDate"      TEXT    NOT NULL,
    "EndDate"        TEXT,
    "Title"          TEXT    NOT NULL,
    "EpisodeTitle"   TEXT,
    "Overview"       TEXT,
    "Genres"         TEXT,
    "ImageUrl"       TEXT,
    "ProductionYear" INTEGER,
    "EpisodeNum"     TEXT,
    "IsNew"          INTEGER NOT NULL DEFAULT 0,
    "IsPremiere"     INTEGER NOT NULL DEFAULT 0,
    "IsRepeat"       INTEGER NOT NULL DEFAULT 0,
    "OfficialRating" TEXT,
    CONSTRAINT "PK_LiveTvPrograms" PRIMARY KEY ("Id"),
    CONSTRAINT "FK_LiveTvPrograms_Channels_ChannelId" FOREIGN KEY ("ChannelId")
        REFERENCES "LiveTvChannels" ("Id") ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS "HermitIX_LiveTvPrograms_ChannelId_StartDate" ON "HermitLiveTvPrograms" ("ChannelId", "StartDate");

CREATE TABLE IF NOT EXISTS "HermitLiveTvRecordings" (
    "Id"            TEXT    NOT NULL,
    "ChannelId"     TEXT    NOT NULL,
    "TimerId"       TEXT,
    "SeriesTimerId" TEXT,
    "Name"          TEXT    NOT NULL DEFAULT '',
    "Overview"      TEXT,
    "StartDate"     TEXT    NOT NULL,
    "EndDate"       TEXT,
    "Status"        TEXT    NOT NULL DEFAULT 'New',
    "Path"          TEXT,
    CONSTRAINT "PK_LiveTvRecordings" PRIMARY KEY ("Id")
);
CREATE INDEX IF NOT EXISTS "HermitIX_LiveTvRecordings_ChannelId" ON "HermitLiveTvRecordings" ("ChannelId");

CREATE TABLE IF NOT EXISTS "HermitLiveTvSeriesTimers" (
    "Id"        TEXT NOT NULL,
    "ChannelId" TEXT,
    "ProgramId" TEXT,
    "Name"      TEXT NOT NULL DEFAULT '',
    "Data"      TEXT NOT NULL,
    CONSTRAINT "PK_LiveTvSeriesTimers" PRIMARY KEY ("Id")
);

CREATE TABLE IF NOT EXISTS "HermitLiveTvTimers" (
    "Id"            TEXT    NOT NULL,
    "ChannelId"     TEXT    NOT NULL,
    "ProgramId"     TEXT,
    "SeriesTimerId" TEXT,
    "Name"          TEXT    NOT NULL DEFAULT '',
    "StartDate"     TEXT    NOT NULL,
    "EndDate"       TEXT,
    "Status"        TEXT    NOT NULL DEFAULT 'New',
    "PrePaddingSeconds"  INTEGER NOT NULL DEFAULT 0,
    "PostPaddingSeconds" INTEGER NOT NULL DEFAULT 0,
    "Data"          TEXT    NOT NULL,
    CONSTRAINT "PK_LiveTvTimers" PRIMARY KEY ("Id")
);
CREATE INDEX IF NOT EXISTS "HermitIX_LiveTvTimers_SeriesTimerId" ON "HermitLiveTvTimers" ("SeriesTimerId");
CREATE INDEX IF NOT EXISTS "HermitIX_LiveTvTimers_StartDate" ON "HermitLiveTvTimers" ("StartDate");

CREATE TABLE IF NOT EXISTS "HermitLiveTvTunerHosts" (
    "Id"   TEXT NOT NULL,
    "Url"  TEXT NOT NULL,
    "Type" TEXT NOT NULL,
    "Data" TEXT NOT NULL,
    CONSTRAINT "PK_LiveTvTunerHosts" PRIMARY KEY ("Id")
);

CREATE TABLE IF NOT EXISTS "HermitPlaybackSessions" (
    "PlaySessionId" TEXT NOT NULL PRIMARY KEY,
    "ItemId" TEXT NOT NULL,
    "UserId" TEXT NOT NULL,
    "Client" TEXT NULL,
    "DeviceId" TEXT NULL,
    -- DirectPlay | DirectStream | Transcode (the final decision sent to the client)
    "PlayMethod" TEXT NOT NULL,
    -- Comma-separated TranscodeReason names; empty for direct play.
    "TranscodeReasons" TEXT NOT NULL DEFAULT '',
    "Container" TEXT NULL,
    "VideoCodec" TEXT NULL,
    "AudioCodec" TEXT NULL,
    "TargetContainer" TEXT NULL,
    "TargetVideoCodec" TEXT NULL,
    "TargetAudioCodec" TEXT NULL,
    "DecidedAt" TEXT NOT NULL,
    "StartedAt" TEXT NULL,
    "StoppedAt" TEXT NULL,
    "PositionTicks" INTEGER NULL,
    "TranscodeWallMs" INTEGER NULL,
    "TranscodeCpuMs" INTEGER NULL,
    "TranscodePeakRssKb" INTEGER NULL,
    "FirstSegmentMs" INTEGER NULL
);
CREATE INDEX IF NOT EXISTS "HermitIX_PlaybackSessions_DecidedAt" ON "HermitPlaybackSessions" ("DecidedAt");

CREATE TABLE IF NOT EXISTS "HermitPlaylistShares" (
    "PlaylistId" TEXT    NOT NULL,
    "UserId"     TEXT    NOT NULL,
    "CanEdit"    INTEGER NOT NULL DEFAULT 0,
    CONSTRAINT "PK_PlaylistShares" PRIMARY KEY ("PlaylistId", "UserId"),
    CONSTRAINT "FK_PlaylistShares_BaseItems_PlaylistId" FOREIGN KEY ("PlaylistId")
        REFERENCES "BaseItems" ("Id") ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS "HermitPlaylists" (
    "PlaylistId"  TEXT NOT NULL,
    "OwnerUserId" TEXT,
    "OpenAccess"  INTEGER NOT NULL DEFAULT 0,
    CONSTRAINT "PK_Playlists" PRIMARY KEY ("PlaylistId"),
    CONSTRAINT "FK_Playlists_BaseItems_PlaylistId" FOREIGN KEY ("PlaylistId")
        REFERENCES "BaseItems" ("Id") ON DELETE CASCADE
);

-- Hermit-only perf indexes on Jellyfin-owned tables (HermitIX_ prefix,
-- EF-invisible, collision-proof against upstream's IX_ namespace).
CREATE INDEX IF NOT EXISTS "HermitIX_BaseItemImageInfos_ItemId_ImageType" ON "BaseItemImageInfos" ("ItemId", "ImageType");
CREATE INDEX IF NOT EXISTS "HermitIX_BaseItemProviders_ProviderId_ItemId_ProviderValue" ON "BaseItemProviders" ("ProviderId", "ItemId", "ProviderValue");
CREATE INDEX IF NOT EXISTS "HermitIX_BaseItems_ExtraType_OwnerId" ON "BaseItems" ("ExtraType", "OwnerId");
CREATE INDEX IF NOT EXISTS "HermitIX_BaseItems_Name" ON "BaseItems" ("Name");
CREATE INDEX IF NOT EXISTS "HermitIX_BaseItems_OwnerId" ON "BaseItems" ("OwnerId");
CREATE INDEX IF NOT EXISTS "HermitIX_BaseItems_SeasonId" ON "BaseItems" ("SeasonId");
CREATE INDEX IF NOT EXISTS "HermitIX_BaseItems_SeriesId" ON "BaseItems" ("SeriesId");
CREATE INDEX IF NOT EXISTS "HermitIX_BaseItems_SeriesName" ON "BaseItems" ("SeriesName");
CREATE INDEX IF NOT EXISTS "HermitIX_BaseItems_TopParentId_IsFolder_IsVirtualItem_DateCreated" ON "BaseItems" ("TopParentId", "IsFolder", "IsVirtualItem", "DateCreated");
CREATE INDEX IF NOT EXISTS "HermitIX_BaseItems_TopParentId_MediaType_IsVirtualItem_DateCreated" ON "BaseItems" ("TopParentId", "MediaType", "IsVirtualItem", "DateCreated");
CREATE INDEX IF NOT EXISTS "HermitIX_BaseItems_TopParentId_Type_IsVirtualItem" ON "BaseItems" ("TopParentId", "Type", "IsVirtualItem") WHERE "PrimaryVersionId" IS NULL AND ("OwnerId" IS NULL OR "ExtraType" IS NOT NULL);
CREATE INDEX IF NOT EXISTS "HermitIX_BaseItems_TopParentId_Type_IsVirtualItem_DateCreated" ON "BaseItems" ("TopParentId", "Type", "IsVirtualItem", "DateCreated");
CREATE INDEX IF NOT EXISTS "HermitIX_BaseItems_Type_CleanName" ON "BaseItems" ("Type", "CleanName");
CREATE INDEX IF NOT EXISTS "HermitIX_BaseItems_Type_SeriesPresentationUniqueKey_ParentIndexNumber_IndexNumber" ON "BaseItems" ("Type", "SeriesPresentationUniqueKey", "ParentIndexNumber", "IndexNumber");
CREATE INDEX IF NOT EXISTS "HermitIX_BaseItems_Type_TopParentId_SortName" ON "BaseItems" ("Type", "TopParentId", "SortName");
CREATE INDEX IF NOT EXISTS "HermitIX_UserData_UserId_IsFavorite_ItemId" ON "UserData" ("UserId", "IsFavorite", "ItemId");
CREATE INDEX IF NOT EXISTS "HermitIX_UserData_UserId_ItemId_LastPlayedDate" ON "UserData" ("UserId", "ItemId", "LastPlayedDate");
CREATE INDEX IF NOT EXISTS "HermitIX_UserData_UserId_Played_ItemId" ON "UserData" ("UserId", "Played", "ItemId");

-- Hermit's own key/value metadata (never read by Jellyfin; Hermit* namespace).
CREATE TABLE IF NOT EXISTS "HermitMeta" (
    "Key"   TEXT NOT NULL PRIMARY KEY,
    "Value" TEXT NOT NULL
);

-- Pin the item-id derivation mode per database. Jellyfin 10.11.8 derives ids
-- case-sensitively with a data-dir-relative rewrite; early Hermit lowercased
-- the path. Databases that already carry lowercase-derived scan ids are
-- grandfathered as 'legacy-lowercase' (their ids must stay stable); fresh and
-- adopted-Jellyfin databases take 'jellyfin-10.11.8' so future scans converge
-- on Jellyfin's ids.
INSERT INTO "HermitMeta" ("Key", "Value")
SELECT 'item_id_derivation',
       CASE
           WHEN EXISTS (SELECT 1 FROM sqlite_master
                        WHERE type = 'table' AND name = '__EFMigrationsHistory')
               THEN 'jellyfin-10.11.8'
           WHEN EXISTS (SELECT 1 FROM "BaseItems"
                        WHERE "Path" IS NOT NULL AND "Type" != 'PLACEHOLDER')
               THEN 'legacy-lowercase'
           ELSE 'jellyfin-10.11.8'
       END
WHERE NOT EXISTS (SELECT 1 FROM "HermitMeta" WHERE "Key" = 'item_id_derivation');
