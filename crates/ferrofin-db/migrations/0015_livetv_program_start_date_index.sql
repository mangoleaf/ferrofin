-- Index the guide by air time (Ferrofin-own table, FerrofinIX_ namespace).
--
-- `FerrofinIX_LiveTvPrograms_ChannelId_StartDate` only serves a query that
-- names its channels. The two guide reads that do not — "On Now"
-- (`/LiveTv/Programs/Recommended?IsAiring=true`, whose predicate is
-- `StartDate <= now AND EndDate >= now`) and an unscoped date window — fell to
-- `SCAN p` plus a temp b-tree for the start-date ORDER BY, so a `Limit=24`
-- request sorted the entire programme table before discarding all but 24 rows.
--
-- Leading with StartDate serves the range *and* the ordering, so the scan stops
-- at the limit; carrying EndDate keeps the airing predicate inside the index.
-- Measured on a 300-channel / 7-day / 50,517-programme guide, alternating legs:
-- "On Now" 16.7/24.0 ms -> 3.0/3.5 ms, unscoped 2h window 24.3/25.1 ms ->
-- 9.2/8.0 ms, and the total-record COUNT(*) 26.5/24.9 ms -> 7.7/7.3 ms.
CREATE INDEX IF NOT EXISTS "FerrofinIX_LiveTvPrograms_StartDate"
    ON "FerrofinLiveTvPrograms" ("StartDate", "EndDate");
