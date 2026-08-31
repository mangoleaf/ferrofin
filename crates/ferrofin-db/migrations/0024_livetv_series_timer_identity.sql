-- A series timer carries the server-minted external id its DTO id was derived
-- from, and the series it was created for; a timer records whether a person
-- created it by hand.
--
-- Upstream, `DefaultLiveTvService.CreateSeriesTimer` mints
-- `info.Id = Guid.NewGuid().ToString("N")` (v10.11.8 DefaultLiveTvService.cs:265)
-- and `LiveTvDtoService.GetSeriesTimerInfoDto` publishes
-- `Id = GetInternalSeriesTimerId(info.Id)` with `ExternalId = info.Id`
-- (LiveTvDtoService.cs:117-124). The two ids therefore have to be stored apart:
-- the row key is the published (internal) id, `ExternalId` is what the fan-out
-- stamps onto each child timer's own derived id.
--
-- `SeriesId` is `program.ExternalSeriesId` at create time
-- (DefaultLiveTvService.cs:268-275); `GetTimersForSeries` keys the guide query on
-- it (DefaultLiveTvService.cs:803-828) and falls back to the name when it is
-- empty, so it must survive a restart rather than be re-derived from a programme
-- that may have aged out of the guide.
--
-- `IsManual` is `TimerInfo.IsManual`: a hand-created timer
-- (DefaultLiveTvService.cs:630) that `ShouldCancelTimerForSeriesTimer`
-- (DefaultLiveTvService.cs:646-649) must never cancel and
-- `UpdateTimersForSeriesTimer` must never reset to `New`.
--
-- All three tables are Ferrofin-owned (`Ferrofin*` namespace), so this is
-- additive and the Jellyfin-pinned schema shape is untouched.
ALTER TABLE "FerrofinLiveTvSeriesTimers" ADD COLUMN "ExternalId" TEXT NOT NULL DEFAULT '';
ALTER TABLE "FerrofinLiveTvSeriesTimers" ADD COLUMN "SeriesId" TEXT;
ALTER TABLE "FerrofinLiveTvTimers" ADD COLUMN "IsManual" INTEGER NOT NULL DEFAULT 0;
