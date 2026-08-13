-- Backfill UserData.LastPlayedDate on played rows that predate the
-- record_playback_start fix. Ferrofin's playback-start path never stamped the
-- column (the stop path deliberately doesn't, mirroring upstream, because
-- upstream stamps at start) — so Next Up's recently-watched filter
-- (HAVING last_played IS NOT NULL AND last_played >= <cutoff>) excluded
-- almost every series a user actually watched. An adopted Jellyfin database
-- always has the column populated, so this only touches Ferrofin-scanned
-- watch history.
--
-- "Now" is the only defensible stand-in (the true watch time was never
-- recorded); it puts previously-watched series back inside the one-year
-- Next Up window. The fractional part is padded to Jellyfin's 7-digit
-- format so the stored strings stay shape-identical to real 10.11.8 rows.
UPDATE "UserData"
SET "LastPlayedDate" = strftime('%Y-%m-%d %H:%M:%f', 'now') || '0000'
WHERE "Played" = 1 AND "LastPlayedDate" IS NULL;
