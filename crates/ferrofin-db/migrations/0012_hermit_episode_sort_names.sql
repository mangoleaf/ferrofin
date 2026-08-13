-- Repair episode/season SortName to Jellyfin's per-kind CreateSortName rule.
--
-- `Episode.CreateSortName()` is the zero-padded season/episode numbers ahead of
-- the title ("001 - 0004 - Title"), and `Season.CreateSortName()` is the padded
-- season number ("0001") — NOT the generic name-derived sort name. Rows scanned
-- before that port carry a title-derived key, so a season's episodes sort
-- alphabetically by title. Clients build their play queue from the season in
-- SortName order, so the queue is scrambled: "next episode" points at the wrong
-- item and, at the alphabetically-last episode, at nothing — a dead Next button
-- and no autoplay.
--
-- A rescan rewrites these too, but a library rescan is slow and this is pure
-- data repair; doing it here means an upgrade fixes existing libraries at boot.
-- The values written are exactly what Jellyfin itself writes, so a swap back
-- stays clean.
--
-- Locked rows (IsLocked = 1) are user-owned metadata and are left alone —
-- the same rule the scan's upsert applies. Rows with an explicit
-- ForcedSortName are skipped: that field, not the derived name, is the user's
-- override (Jellyfin derives SortName from it instead of CreateSortName).

UPDATE "BaseItems"
SET "SortName" =
        CASE WHEN "ParentIndexNumber" IS NOT NULL
             THEN printf('%03d - ', "ParentIndexNumber") ELSE '' END
     || CASE WHEN "IndexNumber" IS NOT NULL
             THEN printf('%04d - ', "IndexNumber") ELSE '' END
     || coalesce("Name", '')
WHERE "Type" = 'MediaBrowser.Controller.Entities.TV.Episode'
  AND "IsLocked" = 0
  AND coalesce("ForcedSortName", '') = ''
  AND ("ParentIndexNumber" IS NOT NULL OR "IndexNumber" IS NOT NULL);

UPDATE "BaseItems"
SET "SortName" = printf('%04d', "IndexNumber")
WHERE "Type" = 'MediaBrowser.Controller.Entities.TV.Season'
  AND "IsLocked" = 0
  AND coalesce("ForcedSortName", '') = ''
  AND "IndexNumber" IS NOT NULL;
