-- PlaylistShares — per-user share permissions for a playlist.
--
-- Backs `GET/POST/DELETE /Playlists/{playlistId}/Users` (the C#
-- `Playlist.Shares` list). One row per (playlist, shared user); `CanEdit`
-- mirrors `PlaylistUserPermissions.CanEdit`. Rows cascade-delete with their
-- playlist's `BaseItems` row.
CREATE TABLE "PlaylistShares" (
    "PlaylistId" TEXT    NOT NULL,
    "UserId"     TEXT    NOT NULL,
    "CanEdit"    INTEGER NOT NULL DEFAULT 0,
    CONSTRAINT "PK_PlaylistShares" PRIMARY KEY ("PlaylistId", "UserId"),
    CONSTRAINT "FK_PlaylistShares_BaseItems_PlaylistId" FOREIGN KEY ("PlaylistId")
        REFERENCES "BaseItems" ("Id") ON DELETE CASCADE
);
