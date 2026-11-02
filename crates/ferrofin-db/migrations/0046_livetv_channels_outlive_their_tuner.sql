-- 0046: Live TV channels outlive their tuner host until the next guide refresh.
--
-- 0029 gave "FerrofinLiveTvChannels"."TunerHostId" a foreign key to
-- "FerrofinLiveTvTunerHosts" with ON DELETE CASCADE, so deleting a tuner host
-- dropped its channels (and, through the programme key, their airings) from
-- GET /LiveTv/Channels at once. Jellyfin's TunerHostManager.DeleteTunerHost only
-- edits the configuration and queues the guide refresh; the channels are listed
-- until that refresh's CleanDatabase removes them. Ferrofin's refresh prunes
-- every channel its lineups no longer emit the same way, so the key goes and the
-- refresh does the removal.
--
-- Rebuild the table without that key. The migration connection runs
-- foreign_keys=OFF (see Database::connect, and 0029), so dropping the old table
-- leaves the programmes alone and their key resolves to the new table by name.
-- The programme -> channel cascade is unchanged. Explicit column lists on both
-- sides, as 0029 has: the fresh and the adopted lineages need not share column
-- order.

CREATE TABLE "FerrofinLiveTvChannels_new" (
    "Id"          TEXT    NOT NULL,
    "TunerHostId" TEXT    NOT NULL,
    "TvgId"       TEXT    NOT NULL DEFAULT '',
    "Name"        TEXT    NOT NULL,
    "Number"      TEXT,
    "ImageUrl"    TEXT,
    "ChannelType" TEXT    NOT NULL DEFAULT 'Tv',
    "StreamUrl"   TEXT    NOT NULL,
    "SortIndex"   INTEGER NOT NULL DEFAULT 0,
    "DateCreated" TEXT,
    "ExternalId"  TEXT    NOT NULL DEFAULT '',
    "IsHd"        INTEGER,
    "VideoCodec"  TEXT,
    "AudioCodec"  TEXT,
    CONSTRAINT "PK_LiveTvChannels" PRIMARY KEY ("Id")
);
INSERT INTO "FerrofinLiveTvChannels_new"
    ("Id","TunerHostId","TvgId","Name","Number","ImageUrl","ChannelType",
     "StreamUrl","SortIndex","DateCreated","ExternalId","IsHd","VideoCodec","AudioCodec")
    SELECT "Id","TunerHostId","TvgId","Name","Number","ImageUrl","ChannelType",
           "StreamUrl","SortIndex","DateCreated","ExternalId","IsHd","VideoCodec","AudioCodec"
    FROM "FerrofinLiveTvChannels";
DROP TABLE "FerrofinLiveTvChannels";
ALTER TABLE "FerrofinLiveTvChannels_new" RENAME TO "FerrofinLiveTvChannels";
CREATE INDEX "FerrofinIX_LiveTvChannels_TunerHostId" ON "FerrofinLiveTvChannels" ("TunerHostId");
CREATE INDEX "FerrofinIX_LiveTvChannels_TvgId" ON "FerrofinLiveTvChannels" ("TvgId");
