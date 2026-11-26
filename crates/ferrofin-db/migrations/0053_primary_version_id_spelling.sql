-- 0053: give every PrimaryVersionId the spelling upstream's Guid conversion gives it.
--
-- A version's "BaseItems"."PrimaryVersionId" names its primary's "Id". Ferrofin's
-- lookups compare the two byte for byte (SQLite's BINARY collation): the listing that
-- hides a version behind its primary, the version lists, the user-data roll-up and the
-- adoption boot's alternate-version backfill. A value in lower case or as 32 undashed
-- characters names the primary for Jellyfin, which reads the column as a Guid, and for
-- none of those, so the version stays a separate library entry.
--
-- A 10.11 source gets the conversion from 0032, which ports
-- 20260215201634_ChangePrimaryVersionIdToGuid (…ChangePrimaryVersionIdToGuid.cs:15-41).
-- A 12.x source has that routine in its history (Jellyfin applied it when it created or
-- upgraded the file) and never passes through 0032's statements, so a value written
-- another way after it ran stays. Jellyfin 12.4 boots it as written and writes it
-- upper-case dashed the next time it saves the item (a scan's save included), since it
-- saves the Guid it read. Ferrofin's scan never writes the column
-- (item_persistence_service.rs), so this runs 0032's four statements, unchanged, once on
-- every source, as 0045 does for OwnerId: N-format → dashed, upper case, anything not 36
-- characters → NULL, the all-zero Guid → NULL. A 12.x source then ends where a 10.11 one
-- does. A no-op on a database 0032 converted or Ferrofin wrote: every value Ferrofin
-- writes is upper-case dashed (guid_to_db), also one the previous version saves during a
-- rolling update.
--
-- Before the boot repairs, so the adoption boot's alternate-version backfill sees the
-- converted value. Not in REBUILD_MIGRATIONS (database.rs): this deletes nothing.

UPDATE "BaseItems"
   SET "PrimaryVersionId" = upper(substr("PrimaryVersionId", 1, 8) || '-' || substr("PrimaryVersionId", 9, 4) || '-'
                             || substr("PrimaryVersionId", 13, 4) || '-' || substr("PrimaryVersionId", 17, 4) || '-'
                             || substr("PrimaryVersionId", 21, 12))
 WHERE "PrimaryVersionId" IS NOT NULL AND length("PrimaryVersionId") = 32;
UPDATE "BaseItems" SET "PrimaryVersionId" = upper("PrimaryVersionId") WHERE "PrimaryVersionId" IS NOT NULL;
UPDATE "BaseItems" SET "PrimaryVersionId" = NULL
 WHERE "PrimaryVersionId" IS NOT NULL AND length("PrimaryVersionId") != 36;
UPDATE "BaseItems" SET "PrimaryVersionId" = NULL
 WHERE "PrimaryVersionId" = '00000000-0000-0000-0000-000000000000';
