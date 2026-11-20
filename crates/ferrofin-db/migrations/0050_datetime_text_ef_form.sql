-- 0050: store every DateTime as Jellyfin's EF provider writes it: the seven-digit tick fraction with its trailing zeros removed, and no '.' for a whole second.
--
-- Jellyfin's database is written through Microsoft.Data.Sqlite's binder, which formats a
-- DateTime as 'yyyy-MM-dd HH:mm:ss.FFFFFFF' (SqliteValueBinder, Microsoft.Data.Sqlite
-- 10.0.14 as Jellyfin 12.4 pins it). The F specifier drops trailing zeros, and the '.'
-- with them when nothing is left: '2020-01-01 00:00:00', '2026-11-04 07:46:26.447945'.
-- Ferrofin wrote seven digits always ('2020-01-01 00:00:00.0000000'), and a by-name row made
-- on demand in RFC 3339 ('2026-11-11T00:54:23.994092718+00:00', sqlx's encoding of a raw
-- DateTime, with 0, 3, 6 or 9 fractional digits). SQLite compares TEXT byte by byte, so one
-- instant in two forms compares unequal: MinPremiereDate dropped an adopted item that
-- premiered at the bound, and after an export Jellyfin's <= comparisons missed Ferrofin's
-- rows (gl-1565). ferrofin_db::store::datetime_to_db now writes Jellyfin's form; this
-- rewrites what Ferrofin stored before.
--
-- Each value is rewritten only when it is in one of Ferrofin's forms and differs from
-- Jellyfin's: a space-separated value whose fraction is all digits and ends in a zero
-- (seven digits, or the three of 0007's strftime), whose trailing zeros and then '.' are
-- trimmed (the '.' stops the trim before the seconds); and an RFC 3339 value at +00:00,
-- which takes the space, keeps at most seven fraction digits (the 100 ns tick, cut as
-- datetime_to_db cuts) and is trimmed the same way. Anything else, a value Jellyfin wrote
-- included, is left as it is, so a second run changes nothing.
--
-- The 21 DateTime columns of Jellyfin's schema, and Ferrofin's own date columns, which are
-- written and compared the same way. One UPDATE per table, touching only the rows that
-- change, keeps the write short.
--
-- During a rolling update the previous version keeps writing seven digits until its pod
-- stops; those rows stay in that form until they are next saved. That is the mixed state an
-- adopted database was already in, and readers parse both forms.

UPDATE "ActivityLogs"
   SET "DateCreated" = CASE WHEN ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateCreated", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("DateCreated", '0'), '.')
            WHEN ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateCreated", 20, length("DateCreated") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("DateCreated", 20, length("DateCreated") - 25) = '' THEN replace(substr("DateCreated", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("DateCreated", 1, 19), 'T', ' ') || '.' || substr(substr("DateCreated", 20, length("DateCreated") - 25), 2, 7), '0'), '.') END
            ELSE "DateCreated" END
 WHERE ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateCreated", 21) NOT GLOB '*[^0-9]*') OR ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateCreated", 20, length("DateCreated") - 25) NOT GLOB '*[^.0-9]*');

UPDATE "ApiKeys"
   SET "DateCreated" = CASE WHEN ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateCreated", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("DateCreated", '0'), '.')
            WHEN ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateCreated", 20, length("DateCreated") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("DateCreated", 20, length("DateCreated") - 25) = '' THEN replace(substr("DateCreated", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("DateCreated", 1, 19), 'T', ' ') || '.' || substr(substr("DateCreated", 20, length("DateCreated") - 25), 2, 7), '0'), '.') END
            ELSE "DateCreated" END,
       "DateLastActivity" = CASE WHEN ("DateLastActivity" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateLastActivity", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("DateLastActivity", '0'), '.')
            WHEN ("DateLastActivity" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateLastActivity", 20, length("DateLastActivity") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("DateLastActivity", 20, length("DateLastActivity") - 25) = '' THEN replace(substr("DateLastActivity", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("DateLastActivity", 1, 19), 'T', ' ') || '.' || substr(substr("DateLastActivity", 20, length("DateLastActivity") - 25), 2, 7), '0'), '.') END
            ELSE "DateLastActivity" END
 WHERE ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateCreated", 21) NOT GLOB '*[^0-9]*') OR ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateCreated", 20, length("DateCreated") - 25) NOT GLOB '*[^.0-9]*')
    OR ("DateLastActivity" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateLastActivity", 21) NOT GLOB '*[^0-9]*') OR ("DateLastActivity" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateLastActivity", 20, length("DateLastActivity") - 25) NOT GLOB '*[^.0-9]*');

UPDATE "BaseItemImageInfos"
   SET "DateModified" = CASE WHEN ("DateModified" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateModified", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("DateModified", '0'), '.')
            WHEN ("DateModified" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateModified", 20, length("DateModified") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("DateModified", 20, length("DateModified") - 25) = '' THEN replace(substr("DateModified", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("DateModified", 1, 19), 'T', ' ') || '.' || substr(substr("DateModified", 20, length("DateModified") - 25), 2, 7), '0'), '.') END
            ELSE "DateModified" END
 WHERE ("DateModified" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateModified", 21) NOT GLOB '*[^0-9]*') OR ("DateModified" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateModified", 20, length("DateModified") - 25) NOT GLOB '*[^.0-9]*');

UPDATE "BaseItems"
   SET "DateCreated" = CASE WHEN ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateCreated", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("DateCreated", '0'), '.')
            WHEN ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateCreated", 20, length("DateCreated") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("DateCreated", 20, length("DateCreated") - 25) = '' THEN replace(substr("DateCreated", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("DateCreated", 1, 19), 'T', ' ') || '.' || substr(substr("DateCreated", 20, length("DateCreated") - 25), 2, 7), '0'), '.') END
            ELSE "DateCreated" END,
       "DateLastMediaAdded" = CASE WHEN ("DateLastMediaAdded" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateLastMediaAdded", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("DateLastMediaAdded", '0'), '.')
            WHEN ("DateLastMediaAdded" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateLastMediaAdded", 20, length("DateLastMediaAdded") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("DateLastMediaAdded", 20, length("DateLastMediaAdded") - 25) = '' THEN replace(substr("DateLastMediaAdded", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("DateLastMediaAdded", 1, 19), 'T', ' ') || '.' || substr(substr("DateLastMediaAdded", 20, length("DateLastMediaAdded") - 25), 2, 7), '0'), '.') END
            ELSE "DateLastMediaAdded" END,
       "DateLastRefreshed" = CASE WHEN ("DateLastRefreshed" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateLastRefreshed", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("DateLastRefreshed", '0'), '.')
            WHEN ("DateLastRefreshed" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateLastRefreshed", 20, length("DateLastRefreshed") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("DateLastRefreshed", 20, length("DateLastRefreshed") - 25) = '' THEN replace(substr("DateLastRefreshed", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("DateLastRefreshed", 1, 19), 'T', ' ') || '.' || substr(substr("DateLastRefreshed", 20, length("DateLastRefreshed") - 25), 2, 7), '0'), '.') END
            ELSE "DateLastRefreshed" END,
       "DateLastSaved" = CASE WHEN ("DateLastSaved" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateLastSaved", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("DateLastSaved", '0'), '.')
            WHEN ("DateLastSaved" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateLastSaved", 20, length("DateLastSaved") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("DateLastSaved", 20, length("DateLastSaved") - 25) = '' THEN replace(substr("DateLastSaved", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("DateLastSaved", 1, 19), 'T', ' ') || '.' || substr(substr("DateLastSaved", 20, length("DateLastSaved") - 25), 2, 7), '0'), '.') END
            ELSE "DateLastSaved" END,
       "DateModified" = CASE WHEN ("DateModified" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateModified", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("DateModified", '0'), '.')
            WHEN ("DateModified" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateModified", 20, length("DateModified") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("DateModified", 20, length("DateModified") - 25) = '' THEN replace(substr("DateModified", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("DateModified", 1, 19), 'T', ' ') || '.' || substr(substr("DateModified", 20, length("DateModified") - 25), 2, 7), '0'), '.') END
            ELSE "DateModified" END,
       "EndDate" = CASE WHEN ("EndDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("EndDate", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("EndDate", '0'), '.')
            WHEN ("EndDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("EndDate", 20, length("EndDate") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("EndDate", 20, length("EndDate") - 25) = '' THEN replace(substr("EndDate", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("EndDate", 1, 19), 'T', ' ') || '.' || substr(substr("EndDate", 20, length("EndDate") - 25), 2, 7), '0'), '.') END
            ELSE "EndDate" END,
       "PremiereDate" = CASE WHEN ("PremiereDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("PremiereDate", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("PremiereDate", '0'), '.')
            WHEN ("PremiereDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("PremiereDate", 20, length("PremiereDate") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("PremiereDate", 20, length("PremiereDate") - 25) = '' THEN replace(substr("PremiereDate", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("PremiereDate", 1, 19), 'T', ' ') || '.' || substr(substr("PremiereDate", 20, length("PremiereDate") - 25), 2, 7), '0'), '.') END
            ELSE "PremiereDate" END,
       "StartDate" = CASE WHEN ("StartDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("StartDate", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("StartDate", '0'), '.')
            WHEN ("StartDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("StartDate", 20, length("StartDate") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("StartDate", 20, length("StartDate") - 25) = '' THEN replace(substr("StartDate", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("StartDate", 1, 19), 'T', ' ') || '.' || substr(substr("StartDate", 20, length("StartDate") - 25), 2, 7), '0'), '.') END
            ELSE "StartDate" END
 WHERE ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateCreated", 21) NOT GLOB '*[^0-9]*') OR ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateCreated", 20, length("DateCreated") - 25) NOT GLOB '*[^.0-9]*')
    OR ("DateLastMediaAdded" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateLastMediaAdded", 21) NOT GLOB '*[^0-9]*') OR ("DateLastMediaAdded" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateLastMediaAdded", 20, length("DateLastMediaAdded") - 25) NOT GLOB '*[^.0-9]*')
    OR ("DateLastRefreshed" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateLastRefreshed", 21) NOT GLOB '*[^0-9]*') OR ("DateLastRefreshed" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateLastRefreshed", 20, length("DateLastRefreshed") - 25) NOT GLOB '*[^.0-9]*')
    OR ("DateLastSaved" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateLastSaved", 21) NOT GLOB '*[^0-9]*') OR ("DateLastSaved" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateLastSaved", 20, length("DateLastSaved") - 25) NOT GLOB '*[^.0-9]*')
    OR ("DateModified" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateModified", 21) NOT GLOB '*[^0-9]*') OR ("DateModified" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateModified", 20, length("DateModified") - 25) NOT GLOB '*[^.0-9]*')
    OR ("EndDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("EndDate", 21) NOT GLOB '*[^0-9]*') OR ("EndDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("EndDate", 20, length("EndDate") - 25) NOT GLOB '*[^.0-9]*')
    OR ("PremiereDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("PremiereDate", 21) NOT GLOB '*[^0-9]*') OR ("PremiereDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("PremiereDate", 20, length("PremiereDate") - 25) NOT GLOB '*[^.0-9]*')
    OR ("StartDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("StartDate", 21) NOT GLOB '*[^0-9]*') OR ("StartDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("StartDate", 20, length("StartDate") - 25) NOT GLOB '*[^.0-9]*');

UPDATE "Chapters"
   SET "ImageDateModified" = CASE WHEN ("ImageDateModified" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("ImageDateModified", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("ImageDateModified", '0'), '.')
            WHEN ("ImageDateModified" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("ImageDateModified", 20, length("ImageDateModified") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("ImageDateModified", 20, length("ImageDateModified") - 25) = '' THEN replace(substr("ImageDateModified", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("ImageDateModified", 1, 19), 'T', ' ') || '.' || substr(substr("ImageDateModified", 20, length("ImageDateModified") - 25), 2, 7), '0'), '.') END
            ELSE "ImageDateModified" END
 WHERE ("ImageDateModified" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("ImageDateModified", 21) NOT GLOB '*[^0-9]*') OR ("ImageDateModified" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("ImageDateModified", 20, length("ImageDateModified") - 25) NOT GLOB '*[^.0-9]*');

UPDATE "Devices"
   SET "DateCreated" = CASE WHEN ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateCreated", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("DateCreated", '0'), '.')
            WHEN ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateCreated", 20, length("DateCreated") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("DateCreated", 20, length("DateCreated") - 25) = '' THEN replace(substr("DateCreated", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("DateCreated", 1, 19), 'T', ' ') || '.' || substr(substr("DateCreated", 20, length("DateCreated") - 25), 2, 7), '0'), '.') END
            ELSE "DateCreated" END,
       "DateModified" = CASE WHEN ("DateModified" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateModified", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("DateModified", '0'), '.')
            WHEN ("DateModified" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateModified", 20, length("DateModified") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("DateModified", 20, length("DateModified") - 25) = '' THEN replace(substr("DateModified", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("DateModified", 1, 19), 'T', ' ') || '.' || substr(substr("DateModified", 20, length("DateModified") - 25), 2, 7), '0'), '.') END
            ELSE "DateModified" END,
       "DateLastActivity" = CASE WHEN ("DateLastActivity" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateLastActivity", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("DateLastActivity", '0'), '.')
            WHEN ("DateLastActivity" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateLastActivity", 20, length("DateLastActivity") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("DateLastActivity", 20, length("DateLastActivity") - 25) = '' THEN replace(substr("DateLastActivity", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("DateLastActivity", 1, 19), 'T', ' ') || '.' || substr(substr("DateLastActivity", 20, length("DateLastActivity") - 25), 2, 7), '0'), '.') END
            ELSE "DateLastActivity" END
 WHERE ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateCreated", 21) NOT GLOB '*[^0-9]*') OR ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateCreated", 20, length("DateCreated") - 25) NOT GLOB '*[^.0-9]*')
    OR ("DateModified" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateModified", 21) NOT GLOB '*[^0-9]*') OR ("DateModified" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateModified", 20, length("DateModified") - 25) NOT GLOB '*[^.0-9]*')
    OR ("DateLastActivity" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateLastActivity", 21) NOT GLOB '*[^0-9]*') OR ("DateLastActivity" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateLastActivity", 20, length("DateLastActivity") - 25) NOT GLOB '*[^.0-9]*');

UPDATE "ImageInfos"
   SET "LastModified" = CASE WHEN ("LastModified" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("LastModified", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("LastModified", '0'), '.')
            WHEN ("LastModified" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("LastModified", 20, length("LastModified") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("LastModified", 20, length("LastModified") - 25) = '' THEN replace(substr("LastModified", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("LastModified", 1, 19), 'T', ' ') || '.' || substr(substr("LastModified", 20, length("LastModified") - 25), 2, 7), '0'), '.') END
            ELSE "LastModified" END
 WHERE ("LastModified" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("LastModified", 21) NOT GLOB '*[^0-9]*') OR ("LastModified" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("LastModified", 20, length("LastModified") - 25) NOT GLOB '*[^.0-9]*');

UPDATE "UserData"
   SET "LastPlayedDate" = CASE WHEN ("LastPlayedDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("LastPlayedDate", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("LastPlayedDate", '0'), '.')
            WHEN ("LastPlayedDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("LastPlayedDate", 20, length("LastPlayedDate") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("LastPlayedDate", 20, length("LastPlayedDate") - 25) = '' THEN replace(substr("LastPlayedDate", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("LastPlayedDate", 1, 19), 'T', ' ') || '.' || substr(substr("LastPlayedDate", 20, length("LastPlayedDate") - 25), 2, 7), '0'), '.') END
            ELSE "LastPlayedDate" END,
       "RetentionDate" = CASE WHEN ("RetentionDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("RetentionDate", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("RetentionDate", '0'), '.')
            WHEN ("RetentionDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("RetentionDate", 20, length("RetentionDate") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("RetentionDate", 20, length("RetentionDate") - 25) = '' THEN replace(substr("RetentionDate", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("RetentionDate", 1, 19), 'T', ' ') || '.' || substr(substr("RetentionDate", 20, length("RetentionDate") - 25), 2, 7), '0'), '.') END
            ELSE "RetentionDate" END
 WHERE ("LastPlayedDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("LastPlayedDate", 21) NOT GLOB '*[^0-9]*') OR ("LastPlayedDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("LastPlayedDate", 20, length("LastPlayedDate") - 25) NOT GLOB '*[^.0-9]*')
    OR ("RetentionDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("RetentionDate", 21) NOT GLOB '*[^0-9]*') OR ("RetentionDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("RetentionDate", 20, length("RetentionDate") - 25) NOT GLOB '*[^.0-9]*');

UPDATE "Users"
   SET "LastActivityDate" = CASE WHEN ("LastActivityDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("LastActivityDate", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("LastActivityDate", '0'), '.')
            WHEN ("LastActivityDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("LastActivityDate", 20, length("LastActivityDate") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("LastActivityDate", 20, length("LastActivityDate") - 25) = '' THEN replace(substr("LastActivityDate", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("LastActivityDate", 1, 19), 'T', ' ') || '.' || substr(substr("LastActivityDate", 20, length("LastActivityDate") - 25), 2, 7), '0'), '.') END
            ELSE "LastActivityDate" END,
       "LastLoginDate" = CASE WHEN ("LastLoginDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("LastLoginDate", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("LastLoginDate", '0'), '.')
            WHEN ("LastLoginDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("LastLoginDate", 20, length("LastLoginDate") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("LastLoginDate", 20, length("LastLoginDate") - 25) = '' THEN replace(substr("LastLoginDate", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("LastLoginDate", 1, 19), 'T', ' ') || '.' || substr(substr("LastLoginDate", 20, length("LastLoginDate") - 25), 2, 7), '0'), '.') END
            ELSE "LastLoginDate" END
 WHERE ("LastActivityDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("LastActivityDate", 21) NOT GLOB '*[^0-9]*') OR ("LastActivityDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("LastActivityDate", 20, length("LastActivityDate") - 25) NOT GLOB '*[^.0-9]*')
    OR ("LastLoginDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("LastLoginDate", 21) NOT GLOB '*[^0-9]*') OR ("LastLoginDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("LastLoginDate", 20, length("LastLoginDate") - 25) NOT GLOB '*[^.0-9]*');

UPDATE "FerrofinExternalIdentities"
   SET "DateCreated" = CASE WHEN ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateCreated", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("DateCreated", '0'), '.')
            WHEN ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateCreated", 20, length("DateCreated") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("DateCreated", 20, length("DateCreated") - 25) = '' THEN replace(substr("DateCreated", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("DateCreated", 1, 19), 'T', ' ') || '.' || substr(substr("DateCreated", 20, length("DateCreated") - 25), 2, 7), '0'), '.') END
            ELSE "DateCreated" END
 WHERE ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateCreated", 21) NOT GLOB '*[^0-9]*') OR ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateCreated", 20, length("DateCreated") - 25) NOT GLOB '*[^.0-9]*');

UPDATE "FerrofinLiveTvChannels"
   SET "DateCreated" = CASE WHEN ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateCreated", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("DateCreated", '0'), '.')
            WHEN ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateCreated", 20, length("DateCreated") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("DateCreated", 20, length("DateCreated") - 25) = '' THEN replace(substr("DateCreated", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("DateCreated", 1, 19), 'T', ' ') || '.' || substr(substr("DateCreated", 20, length("DateCreated") - 25), 2, 7), '0'), '.') END
            ELSE "DateCreated" END
 WHERE ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateCreated", 21) NOT GLOB '*[^0-9]*') OR ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateCreated", 20, length("DateCreated") - 25) NOT GLOB '*[^.0-9]*');

UPDATE "FerrofinLiveTvPrograms"
   SET "StartDate" = CASE WHEN ("StartDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("StartDate", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("StartDate", '0'), '.')
            WHEN ("StartDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("StartDate", 20, length("StartDate") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("StartDate", 20, length("StartDate") - 25) = '' THEN replace(substr("StartDate", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("StartDate", 1, 19), 'T', ' ') || '.' || substr(substr("StartDate", 20, length("StartDate") - 25), 2, 7), '0'), '.') END
            ELSE "StartDate" END,
       "EndDate" = CASE WHEN ("EndDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("EndDate", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("EndDate", '0'), '.')
            WHEN ("EndDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("EndDate", 20, length("EndDate") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("EndDate", 20, length("EndDate") - 25) = '' THEN replace(substr("EndDate", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("EndDate", 1, 19), 'T', ' ') || '.' || substr(substr("EndDate", 20, length("EndDate") - 25), 2, 7), '0'), '.') END
            ELSE "EndDate" END,
       "DateCreated" = CASE WHEN ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateCreated", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("DateCreated", '0'), '.')
            WHEN ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateCreated", 20, length("DateCreated") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("DateCreated", 20, length("DateCreated") - 25) = '' THEN replace(substr("DateCreated", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("DateCreated", 1, 19), 'T', ' ') || '.' || substr(substr("DateCreated", 20, length("DateCreated") - 25), 2, 7), '0'), '.') END
            ELSE "DateCreated" END
 WHERE ("StartDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("StartDate", 21) NOT GLOB '*[^0-9]*') OR ("StartDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("StartDate", 20, length("StartDate") - 25) NOT GLOB '*[^.0-9]*')
    OR ("EndDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("EndDate", 21) NOT GLOB '*[^0-9]*') OR ("EndDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("EndDate", 20, length("EndDate") - 25) NOT GLOB '*[^.0-9]*')
    OR ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateCreated", 21) NOT GLOB '*[^0-9]*') OR ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateCreated", 20, length("DateCreated") - 25) NOT GLOB '*[^.0-9]*');

UPDATE "FerrofinLiveTvRecordings"
   SET "StartDate" = CASE WHEN ("StartDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("StartDate", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("StartDate", '0'), '.')
            WHEN ("StartDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("StartDate", 20, length("StartDate") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("StartDate", 20, length("StartDate") - 25) = '' THEN replace(substr("StartDate", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("StartDate", 1, 19), 'T', ' ') || '.' || substr(substr("StartDate", 20, length("StartDate") - 25), 2, 7), '0'), '.') END
            ELSE "StartDate" END,
       "EndDate" = CASE WHEN ("EndDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("EndDate", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("EndDate", '0'), '.')
            WHEN ("EndDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("EndDate", 20, length("EndDate") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("EndDate", 20, length("EndDate") - 25) = '' THEN replace(substr("EndDate", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("EndDate", 1, 19), 'T', ' ') || '.' || substr(substr("EndDate", 20, length("EndDate") - 25), 2, 7), '0'), '.') END
            ELSE "EndDate" END,
       "DateCreated" = CASE WHEN ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateCreated", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("DateCreated", '0'), '.')
            WHEN ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateCreated", 20, length("DateCreated") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("DateCreated", 20, length("DateCreated") - 25) = '' THEN replace(substr("DateCreated", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("DateCreated", 1, 19), 'T', ' ') || '.' || substr(substr("DateCreated", 20, length("DateCreated") - 25), 2, 7), '0'), '.') END
            ELSE "DateCreated" END
 WHERE ("StartDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("StartDate", 21) NOT GLOB '*[^0-9]*') OR ("StartDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("StartDate", 20, length("StartDate") - 25) NOT GLOB '*[^.0-9]*')
    OR ("EndDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("EndDate", 21) NOT GLOB '*[^0-9]*') OR ("EndDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("EndDate", 20, length("EndDate") - 25) NOT GLOB '*[^.0-9]*')
    OR ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DateCreated", 21) NOT GLOB '*[^0-9]*') OR ("DateCreated" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DateCreated", 20, length("DateCreated") - 25) NOT GLOB '*[^.0-9]*');

UPDATE "FerrofinLiveTvTimers"
   SET "StartDate" = CASE WHEN ("StartDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("StartDate", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("StartDate", '0'), '.')
            WHEN ("StartDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("StartDate", 20, length("StartDate") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("StartDate", 20, length("StartDate") - 25) = '' THEN replace(substr("StartDate", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("StartDate", 1, 19), 'T', ' ') || '.' || substr(substr("StartDate", 20, length("StartDate") - 25), 2, 7), '0'), '.') END
            ELSE "StartDate" END,
       "EndDate" = CASE WHEN ("EndDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("EndDate", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("EndDate", '0'), '.')
            WHEN ("EndDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("EndDate", 20, length("EndDate") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("EndDate", 20, length("EndDate") - 25) = '' THEN replace(substr("EndDate", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("EndDate", 1, 19), 'T', ' ') || '.' || substr(substr("EndDate", 20, length("EndDate") - 25), 2, 7), '0'), '.') END
            ELSE "EndDate" END
 WHERE ("StartDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("StartDate", 21) NOT GLOB '*[^0-9]*') OR ("StartDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("StartDate", 20, length("StartDate") - 25) NOT GLOB '*[^.0-9]*')
    OR ("EndDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("EndDate", 21) NOT GLOB '*[^0-9]*') OR ("EndDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("EndDate", 20, length("EndDate") - 25) NOT GLOB '*[^.0-9]*');

UPDATE "FerrofinPlaybackSessions"
   SET "DecidedAt" = CASE WHEN ("DecidedAt" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DecidedAt", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("DecidedAt", '0'), '.')
            WHEN ("DecidedAt" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DecidedAt", 20, length("DecidedAt") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("DecidedAt", 20, length("DecidedAt") - 25) = '' THEN replace(substr("DecidedAt", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("DecidedAt", 1, 19), 'T', ' ') || '.' || substr(substr("DecidedAt", 20, length("DecidedAt") - 25), 2, 7), '0'), '.') END
            ELSE "DecidedAt" END,
       "StartedAt" = CASE WHEN ("StartedAt" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("StartedAt", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("StartedAt", '0'), '.')
            WHEN ("StartedAt" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("StartedAt", 20, length("StartedAt") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("StartedAt", 20, length("StartedAt") - 25) = '' THEN replace(substr("StartedAt", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("StartedAt", 1, 19), 'T', ' ') || '.' || substr(substr("StartedAt", 20, length("StartedAt") - 25), 2, 7), '0'), '.') END
            ELSE "StartedAt" END,
       "StoppedAt" = CASE WHEN ("StoppedAt" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("StoppedAt", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("StoppedAt", '0'), '.')
            WHEN ("StoppedAt" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("StoppedAt", 20, length("StoppedAt") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("StoppedAt", 20, length("StoppedAt") - 25) = '' THEN replace(substr("StoppedAt", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("StoppedAt", 1, 19), 'T', ' ') || '.' || substr(substr("StoppedAt", 20, length("StoppedAt") - 25), 2, 7), '0'), '.') END
            ELSE "StoppedAt" END
 WHERE ("DecidedAt" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("DecidedAt", 21) NOT GLOB '*[^0-9]*') OR ("DecidedAt" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("DecidedAt", 20, length("DecidedAt") - 25) NOT GLOB '*[^.0-9]*')
    OR ("StartedAt" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("StartedAt", 21) NOT GLOB '*[^0-9]*') OR ("StartedAt" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("StartedAt", 20, length("StartedAt") - 25) NOT GLOB '*[^.0-9]*')
    OR ("StoppedAt" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("StoppedAt", 21) NOT GLOB '*[^0-9]*') OR ("StoppedAt" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("StoppedAt", 20, length("StoppedAt") - 25) NOT GLOB '*[^.0-9]*');

UPDATE "FerrofinPluginOfferedItems"
   SET "OfferedAt" = CASE WHEN ("OfferedAt" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("OfferedAt", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("OfferedAt", '0'), '.')
            WHEN ("OfferedAt" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("OfferedAt", 20, length("OfferedAt") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("OfferedAt", 20, length("OfferedAt") - 25) = '' THEN replace(substr("OfferedAt", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("OfferedAt", 1, 19), 'T', ' ') || '.' || substr(substr("OfferedAt", 20, length("OfferedAt") - 25), 2, 7), '0'), '.') END
            ELSE "OfferedAt" END
 WHERE ("OfferedAt" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("OfferedAt", 21) NOT GLOB '*[^0-9]*') OR ("OfferedAt" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("OfferedAt", 20, length("OfferedAt") - 25) NOT GLOB '*[^.0-9]*');

UPDATE "FerrofinUserDataRetentionSnapshots"
   SET "LastPlayedDate" = CASE WHEN ("LastPlayedDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("LastPlayedDate", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("LastPlayedDate", '0'), '.')
            WHEN ("LastPlayedDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("LastPlayedDate", 20, length("LastPlayedDate") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("LastPlayedDate", 20, length("LastPlayedDate") - 25) = '' THEN replace(substr("LastPlayedDate", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("LastPlayedDate", 1, 19), 'T', ' ') || '.' || substr(substr("LastPlayedDate", 20, length("LastPlayedDate") - 25), 2, 7), '0'), '.') END
            ELSE "LastPlayedDate" END,
       "RetentionDate" = CASE WHEN ("RetentionDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("RetentionDate", 21) NOT GLOB '*[^0-9]*') THEN rtrim(rtrim("RetentionDate", '0'), '.')
            WHEN ("RetentionDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("RetentionDate", 20, length("RetentionDate") - 25) NOT GLOB '*[^.0-9]*') THEN CASE WHEN substr("RetentionDate", 20, length("RetentionDate") - 25) = '' THEN replace(substr("RetentionDate", 1, 19), 'T', ' ')
                 ELSE rtrim(rtrim(replace(substr("RetentionDate", 1, 19), 'T', ' ') || '.' || substr(substr("RetentionDate", 20, length("RetentionDate") - 25), 2, 7), '0'), '.') END
            ELSE "RetentionDate" END
 WHERE ("LastPlayedDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("LastPlayedDate", 21) NOT GLOB '*[^0-9]*') OR ("LastPlayedDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("LastPlayedDate", 20, length("LastPlayedDate") - 25) NOT GLOB '*[^.0-9]*')
    OR ("RetentionDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9].*0' AND substr("RetentionDate", 21) NOT GLOB '*[^0-9]*') OR ("RetentionDate" GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*+00:00' AND substr("RetentionDate", 20, length("RetentionDate") - 25) NOT GLOB '*[^.0-9]*');
