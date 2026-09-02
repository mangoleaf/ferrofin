-- Repoint the two Live TV foreign keys that still name pre-rename tables.
--
-- 0009 copied 0003's DDL verbatim — including FK targets "LiveTvTunerHosts"
-- and "LiveTvChannels", names that 0007 had ALREADY renamed away on the fresh
-- path. On a fresh database 0009's CREATE IF NOT EXISTS is a no-op (0007's
-- rename rewrote the real tables' FK texts), so the stale targets were
-- invisible. On an ADOPTED Jellyfin database 0001-0007 are baselined, 0009's
-- DDL is what actually runs, and the FKs dangle: with foreign_keys=ON every
-- channel INSERT fails with `no such table: main.LiveTvTunerHosts` — Live TV
-- is dead on exactly the adopt-in-place path the drop-in contract promises.
--
-- Rebuild both tables with the FK targets they were always meant to have.
-- SQLite cannot edit an FK in place; the rebuild is safe here because the
-- migration connection runs foreign_keys=OFF (see Database::connect: a rebuild
-- under FK enforcement cascade-deletes dependents — learned on 0007) and the
-- post-migration foreign_key_check is the seatbelt. Explicit column lists on
-- both sides: the fresh lineage (0003 base + ALTERs) and the adopted lineage
-- (0009 base + ALTERs) carry the same columns, not necessarily the same order.

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
    CONSTRAINT "PK_LiveTvChannels" PRIMARY KEY ("Id"),
    CONSTRAINT "FK_LiveTvChannels_TunerHosts_TunerHostId" FOREIGN KEY ("TunerHostId")
        REFERENCES "FerrofinLiveTvTunerHosts" ("Id") ON DELETE CASCADE
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

CREATE TABLE "FerrofinLiveTvPrograms_new" (
    "Id"               TEXT    NOT NULL,
    "ChannelId"        TEXT    NOT NULL,
    "StartDate"        TEXT    NOT NULL,
    "EndDate"          TEXT,
    "Title"            TEXT    NOT NULL,
    "EpisodeTitle"     TEXT,
    "Overview"         TEXT,
    "Genres"           TEXT,
    "ImageUrl"         TEXT,
    "ProductionYear"   INTEGER,
    "EpisodeNum"       TEXT,
    "IsNew"            INTEGER NOT NULL DEFAULT 0,
    "IsPremiere"       INTEGER NOT NULL DEFAULT 0,
    "IsRepeat"         INTEGER NOT NULL DEFAULT 0,
    "OfficialRating"   TEXT,
    "IsMovie"          INTEGER NOT NULL DEFAULT 0,
    "IsSeries"         INTEGER NOT NULL DEFAULT 0,
    "IsNews"           INTEGER NOT NULL DEFAULT 0,
    "IsKids"           INTEGER NOT NULL DEFAULT 0,
    "IsSports"         INTEGER NOT NULL DEFAULT 0,
    "IsLive"           INTEGER NOT NULL DEFAULT 0,
    "ExternalId"       TEXT,
    "ExternalSeriesId" TEXT,
    "SeasonNumber"     INTEGER,
    "EpisodeNumber"    INTEGER,
    "DateCreated"      TEXT,
    "ShowId"           TEXT,
    CONSTRAINT "PK_LiveTvPrograms" PRIMARY KEY ("Id"),
    CONSTRAINT "FK_LiveTvPrograms_Channels_ChannelId" FOREIGN KEY ("ChannelId")
        REFERENCES "FerrofinLiveTvChannels" ("Id") ON DELETE CASCADE
);
INSERT INTO "FerrofinLiveTvPrograms_new"
    ("Id","ChannelId","StartDate","EndDate","Title","EpisodeTitle","Overview","Genres",
     "ImageUrl","ProductionYear","EpisodeNum","IsNew","IsPremiere","IsRepeat","OfficialRating",
     "IsMovie","IsSeries","IsNews","IsKids","IsSports","IsLive","ExternalId","ExternalSeriesId",
     "SeasonNumber","EpisodeNumber","DateCreated","ShowId")
    SELECT "Id","ChannelId","StartDate","EndDate","Title","EpisodeTitle","Overview","Genres",
           "ImageUrl","ProductionYear","EpisodeNum","IsNew","IsPremiere","IsRepeat","OfficialRating",
           "IsMovie","IsSeries","IsNews","IsKids","IsSports","IsLive","ExternalId","ExternalSeriesId",
           "SeasonNumber","EpisodeNumber","DateCreated","ShowId"
    FROM "FerrofinLiveTvPrograms";
DROP TABLE "FerrofinLiveTvPrograms";
ALTER TABLE "FerrofinLiveTvPrograms_new" RENAME TO "FerrofinLiveTvPrograms";
CREATE INDEX "FerrofinIX_LiveTvPrograms_ChannelId_StartDate" ON "FerrofinLiveTvPrograms" ("ChannelId", "StartDate");
CREATE INDEX "FerrofinIX_LiveTvPrograms_StartDate"
    ON "FerrofinLiveTvPrograms" ("StartDate", "EndDate");
