-- Live TV — tuner hosts, listing providers, and the channel/guide cache.
--
-- An M3U tuner host supplies a channel lineup; an XMLTV listing provider
-- supplies the guide. A refresh fetches both, writing `LiveTvChannels` (one row
-- per tuner channel) and `LiveTvPrograms` (the EPG airings). Programmes bind to
-- channels by the tuner's `tvg-id` (stored as `TvgId`), which equals the XMLTV
-- `<channel id>`. `Data` columns hold the full `TunerHostInfo`/
-- `ListingsProviderInfo` DTO as JSON so a GET round-trips exactly what was POSTed.

CREATE TABLE "LiveTvTunerHosts" (
    "Id"   TEXT NOT NULL,
    "Url"  TEXT NOT NULL,
    "Type" TEXT NOT NULL,
    "Data" TEXT NOT NULL,
    CONSTRAINT "PK_LiveTvTunerHosts" PRIMARY KEY ("Id")
);

CREATE TABLE "LiveTvListingProviders" (
    "Id"   TEXT NOT NULL,
    "Type" TEXT NOT NULL,
    "Path" TEXT NOT NULL DEFAULT '',
    "Data" TEXT NOT NULL,
    CONSTRAINT "PK_LiveTvListingProviders" PRIMARY KEY ("Id")
);

CREATE TABLE "LiveTvChannels" (
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

CREATE INDEX "IX_LiveTvChannels_TunerHostId" ON "LiveTvChannels" ("TunerHostId");
CREATE INDEX "IX_LiveTvChannels_TvgId" ON "LiveTvChannels" ("TvgId");

CREATE TABLE "LiveTvPrograms" (
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

CREATE INDEX "IX_LiveTvPrograms_ChannelId_StartDate"
    ON "LiveTvPrograms" ("ChannelId", "StartDate");
