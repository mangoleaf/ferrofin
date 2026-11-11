-- 0051: store the listing fields GuideManager.GetProgram puts on a guide programme item and
-- FerrofinLiveTvPrograms had no column for: the listing's etag (XMLTV's xmltv-sha256-v1 hash, or
-- Schedules Direct's md5), which becomes the item's ProgramEtag provider id; and Schedules
-- Direct's audio format (ProgramAudio), HD flag (a 1280x720 item), original air date (the item's
-- PremiereDate) and Zap2It series id (its SeriesZap2It provider id). Ferrofin-own table, so the
-- columns are additive and nullable: a row the previous version writes during a rolling upgrade
-- has none of them until the next guide refresh rewrites it.
ALTER TABLE "FerrofinLiveTvPrograms" ADD COLUMN "Etag" TEXT NULL;
ALTER TABLE "FerrofinLiveTvPrograms" ADD COLUMN "Audio" INTEGER NULL;
ALTER TABLE "FerrofinLiveTvPrograms" ADD COLUMN "IsHD" INTEGER NULL;
ALTER TABLE "FerrofinLiveTvPrograms" ADD COLUMN "OriginalAirDate" TEXT NULL;
ALTER TABLE "FerrofinLiveTvPrograms" ADD COLUMN "SeriesZap2It" TEXT NULL;
