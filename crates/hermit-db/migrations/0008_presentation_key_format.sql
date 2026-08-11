-- 0008: PresentationUniqueKey in Jellyfin's N-format.
--
-- Jellyfin stores presentation keys as the GUID's lowercase un-hyphenated
-- N-format (`00176712ba4bf7369b7b1c479e852f86`); Hermit's scanner used to
-- write the dashed lowercase form, which splits series grouping when a
-- database moves between the two servers. Normalize existing dashed-GUID
-- values in place — guarded on the exact dashed-GUID shape so composite or
-- already-N-format keys (including everything in an adopted Jellyfin
-- database) pass through untouched. Idempotent; safe on both Hermit-native
-- and adopted databases.

UPDATE "BaseItems"
SET "PresentationUniqueKey" = lower(replace("PresentationUniqueKey", '-', ''))
WHERE "PresentationUniqueKey" IS NOT NULL
  AND length("PresentationUniqueKey") = 36
  AND substr("PresentationUniqueKey", 9, 1) = '-'
  AND substr("PresentationUniqueKey", 14, 1) = '-';

UPDATE "BaseItems"
SET "SeriesPresentationUniqueKey" = lower(replace("SeriesPresentationUniqueKey", '-', ''))
WHERE "SeriesPresentationUniqueKey" IS NOT NULL
  AND length("SeriesPresentationUniqueKey") = 36
  AND substr("SeriesPresentationUniqueKey", 9, 1) = '-'
  AND substr("SeriesPresentationUniqueKey", 14, 1) = '-';
