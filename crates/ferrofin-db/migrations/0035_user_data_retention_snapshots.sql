-- Keep source identity and all aliases together after BaseItems disappears.
-- UserData remains a Jellyfin-compatible mirror; these additive tables also
-- preserve snapshots whose bare provider keys collide in that mirror.
CREATE TABLE "FerrofinUserDataRetentionSnapshots" (
    "SnapshotId" INTEGER PRIMARY KEY AUTOINCREMENT,
    "ItemId" TEXT NOT NULL,
    "UserId" TEXT NOT NULL REFERENCES "Users" ("Id") ON DELETE CASCADE,
    "CustomDataKey" TEXT NOT NULL,
    "AudioStreamIndex" INTEGER,
    "IsFavorite" INTEGER NOT NULL,
    "LastPlayedDate" TEXT,
    "Likes" INTEGER,
    "PlayCount" INTEGER NOT NULL,
    "PlaybackPositionTicks" INTEGER NOT NULL,
    "Played" INTEGER NOT NULL,
    "Rating" REAL,
    "RetentionDate" TEXT NOT NULL,
    "SubtitleStreamIndex" INTEGER
);
CREATE INDEX "FerrofinIX_RetentionSnapshots_UserId"
    ON "FerrofinUserDataRetentionSnapshots" ("UserId");
CREATE INDEX "FerrofinIX_RetentionSnapshots_RetentionDate"
    ON "FerrofinUserDataRetentionSnapshots" ("RetentionDate");

CREATE TABLE "FerrofinUserDataRetentionKeys" (
    "SnapshotId" INTEGER NOT NULL REFERENCES "FerrofinUserDataRetentionSnapshots" ("SnapshotId") ON DELETE CASCADE,
    "Kind" TEXT NOT NULL CHECK ("Kind" IN ('identity', 'mirror')),
    "Key" TEXT NOT NULL,
    PRIMARY KEY ("SnapshotId", "Kind", "Key")
);
CREATE INDEX "FerrofinIX_RetentionKeys_Kind_Key"
    ON "FerrofinUserDataRetentionKeys" ("Kind", "Key", "SnapshotId");
