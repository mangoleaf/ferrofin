-- Playlist ownership + open-access metadata (C# `Playlist.OwnerUserId` /
-- `Playlist.OpenAccess`).
--
-- A side table rather than columns on `BaseItems`:
--   * `BaseItems.OwnerId` cannot hold the owner *user* — it is a self-referential
--     item FK (`FK_BaseItems_BaseItems_OwnerId`), and the query translator
--     treats `OwnerId IS NOT NULL` rows as extras, excluding them from listings.
--   * `OwnerUserId` is deliberately NOT foreign-keyed to `Users`: Jellyfin
--     handles user deletion by transferring/deleting owned playlists
--     (`RemovePlaylists`), which Hermit defers — an FK would make user deletion
--     fail instead.
--
-- A playlist with no row here (created before this migration, or by an API key
-- with no user) is a legacy row: visible to and editable by every user.
CREATE TABLE "Playlists" (
    "PlaylistId"  TEXT NOT NULL,
    "OwnerUserId" TEXT,
    "OpenAccess"  INTEGER NOT NULL DEFAULT 0,
    CONSTRAINT "PK_Playlists" PRIMARY KEY ("PlaylistId"),
    CONSTRAINT "FK_Playlists_BaseItems_PlaylistId" FOREIGN KEY ("PlaylistId")
        REFERENCES "BaseItems" ("Id") ON DELETE CASCADE
);
