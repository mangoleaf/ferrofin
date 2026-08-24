-- Live TV programme classification — the flags Jellyfin's `XmlTvListingsProvider`
-- derives per airing (`GetProgramInfo`) and `GuideManager.GetProgram` turns into
-- the programme's tags/flags: movie/series/news/kids/sports from the provider's
-- category lists, live, and the external ids the guide carries. Without them a
-- programme DTO cannot say IsNews/IsMovie/IsSeries, and the guide's
-- IsMovie/IsSeries/IsNews/IsKids/IsSports filters have nothing to match on.
--
-- Ferrofin-owned table (`FerrofinLiveTvPrograms`), so the columns are additive
-- and the Jellyfin-pinned schema shape is untouched.
ALTER TABLE "FerrofinLiveTvPrograms" ADD COLUMN "IsMovie"  INTEGER NOT NULL DEFAULT 0;
ALTER TABLE "FerrofinLiveTvPrograms" ADD COLUMN "IsSeries" INTEGER NOT NULL DEFAULT 0;
ALTER TABLE "FerrofinLiveTvPrograms" ADD COLUMN "IsNews"   INTEGER NOT NULL DEFAULT 0;
ALTER TABLE "FerrofinLiveTvPrograms" ADD COLUMN "IsKids"   INTEGER NOT NULL DEFAULT 0;
ALTER TABLE "FerrofinLiveTvPrograms" ADD COLUMN "IsSports" INTEGER NOT NULL DEFAULT 0;
ALTER TABLE "FerrofinLiveTvPrograms" ADD COLUMN "IsLive"   INTEGER NOT NULL DEFAULT 0;
-- The listing's own programme id (`ProgramInfo.Id` = "{channelId}_{start:O}") and
-- series id (MD5 of the title when the airing is an episode).
ALTER TABLE "FerrofinLiveTvPrograms" ADD COLUMN "ExternalId"       TEXT;
ALTER TABLE "FerrofinLiveTvPrograms" ADD COLUMN "ExternalSeriesId" TEXT;
-- Season/episode numbers from `<episode-num>` (`xmltv_ns`, 0-based in the file,
-- or `SxxExx`), stored 1-based as `ProgramInfo.SeasonNumber`/`EpisodeNumber`.
ALTER TABLE "FerrofinLiveTvPrograms" ADD COLUMN "SeasonNumber"  INTEGER;
ALTER TABLE "FerrofinLiveTvPrograms" ADD COLUMN "EpisodeNumber" INTEGER;
