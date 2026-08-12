-- Live TV DVR — recording timers, series timers, and recordings.
--
-- A timer schedules one recording (a channel + time window); a series timer is a
-- rule that expands into timers as matching programmes appear in the guide. A
-- recording is the captured file (in progress or complete). `Data` holds the full
-- `TimerInfoDto` / `SeriesTimerInfoDto` DTO as JSON so a GET round-trips exactly;
-- the promoted columns are what the scheduler queries.

CREATE TABLE "LiveTvSeriesTimers" (
    "Id"        TEXT NOT NULL,
    "ChannelId" TEXT,
    "ProgramId" TEXT,
    "Name"      TEXT NOT NULL DEFAULT '',
    "Data"      TEXT NOT NULL,
    CONSTRAINT "PK_LiveTvSeriesTimers" PRIMARY KEY ("Id")
);

CREATE TABLE "LiveTvTimers" (
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

CREATE INDEX "IX_LiveTvTimers_StartDate" ON "LiveTvTimers" ("StartDate");
CREATE INDEX "IX_LiveTvTimers_SeriesTimerId" ON "LiveTvTimers" ("SeriesTimerId");

CREATE TABLE "LiveTvRecordings" (
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

CREATE INDEX "IX_LiveTvRecordings_ChannelId" ON "LiveTvRecordings" ("ChannelId");
