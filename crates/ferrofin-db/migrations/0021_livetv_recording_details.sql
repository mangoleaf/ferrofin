-- Live TV DVR — the programme facts a captured recording carries.
--
-- `FerrofinLiveTvRecordings` held only the timer's own columns, which is enough
-- to list a recording but not to project it the way Jellyfin's recordings (which
-- are library `Video` items) appear: the client shows the episode title, the
-- year, the news/movie/series/kids/sports flags and a completion percentage
-- while a capture is in progress, and orders the list by when it was made.
-- These columns are what `RecordingsManager.RecordStream` knows at the moment
-- the capture starts (`CopyProgramInfoToTimerInfo`'s programme facts), and what
-- `LiveTvManager.AddInfoToRecordingDto` reads back out.
--
-- Ferrofin-owned table, so the columns are additive and namespaced; a Jellyfin
-- database never sees them and swapping back stays safe.

ALTER TABLE "FerrofinLiveTvRecordings" ADD COLUMN "DateCreated"        TEXT;
ALTER TABLE "FerrofinLiveTvRecordings" ADD COLUMN "EpisodeTitle"       TEXT;
ALTER TABLE "FerrofinLiveTvRecordings" ADD COLUMN "ProductionYear"     INTEGER;
ALTER TABLE "FerrofinLiveTvRecordings" ADD COLUMN "SeasonNumber"       INTEGER;
ALTER TABLE "FerrofinLiveTvRecordings" ADD COLUMN "EpisodeNumber"      INTEGER;
ALTER TABLE "FerrofinLiveTvRecordings" ADD COLUMN "ProgramId"          TEXT;
ALTER TABLE "FerrofinLiveTvRecordings" ADD COLUMN "ExternalProgramId"  TEXT;
ALTER TABLE "FerrofinLiveTvRecordings" ADD COLUMN "PrePaddingSeconds"  INTEGER NOT NULL DEFAULT 0;
ALTER TABLE "FerrofinLiveTvRecordings" ADD COLUMN "PostPaddingSeconds" INTEGER NOT NULL DEFAULT 0;
ALTER TABLE "FerrofinLiveTvRecordings" ADD COLUMN "IsMovie"    INTEGER NOT NULL DEFAULT 0;
ALTER TABLE "FerrofinLiveTvRecordings" ADD COLUMN "IsSeries"   INTEGER NOT NULL DEFAULT 0;
ALTER TABLE "FerrofinLiveTvRecordings" ADD COLUMN "IsNews"     INTEGER NOT NULL DEFAULT 0;
ALTER TABLE "FerrofinLiveTvRecordings" ADD COLUMN "IsKids"     INTEGER NOT NULL DEFAULT 0;
ALTER TABLE "FerrofinLiveTvRecordings" ADD COLUMN "IsSports"   INTEGER NOT NULL DEFAULT 0;
ALTER TABLE "FerrofinLiveTvRecordings" ADD COLUMN "IsLive"     INTEGER NOT NULL DEFAULT 0;
ALTER TABLE "FerrofinLiveTvRecordings" ADD COLUMN "IsRepeat"   INTEGER NOT NULL DEFAULT 0;
ALTER TABLE "FerrofinLiveTvRecordings" ADD COLUMN "IsPremiere" INTEGER NOT NULL DEFAULT 0;

-- The recordings list is ordered newest-first, and `isInProgress=true` is the
-- filter the client polls while a capture runs.
CREATE INDEX IF NOT EXISTS "FerrofinIX_LiveTvRecordings_Status_DateCreated"
    ON "FerrofinLiveTvRecordings" ("Status", "DateCreated");
