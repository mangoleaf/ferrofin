-- 0055: add FerrofinPendingChildMetadata, the music albums whose children-derived tags a library scan owes.
--
-- Jellyfin derives an album's artists, album artists, genres, studios and dates from its tracks
-- in the same refresh that saves the album for a change of its own, such as a new folder.jpg
-- (MetadataService.RefreshMetadata, AlbumMetadataService.UpdateMetadataFromChildren). Ferrofin's
-- scan saves the album in its walk and derives those tags in the music pass after it, so a scan
-- stopped between the two (cancelled, or the server restarted) left the album saved and its tags
-- never derived: the next scan finds the album unchanged and derives nothing. The walk now records
-- the album here before it saves it, the music pass deletes the row once it has derived the tags,
-- and a scan that finds a row derives them whether or not the album changed.
--
-- One row per album, its ItemId stored as the BaseItems Id is (uppercase, hyphenated). The key is
-- the only index: the scan reads the whole table once and deletes by key, and the cascade that
-- removes a deleted album's row is a key lookup too.
--
-- Purely additive: the table starts empty and the previous version never reads it. An album the
-- previous version saves while both run is not recorded, as before this migration.
CREATE TABLE "FerrofinPendingChildMetadata" (
    "ItemId" TEXT NOT NULL PRIMARY KEY REFERENCES "BaseItems" ("Id") ON DELETE CASCADE
) WITHOUT ROWID;
