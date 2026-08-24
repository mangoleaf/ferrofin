-- Guide rows carry their creation instant, the way `GuideManager` stamps
-- `DateCreated = DateTime.UtcNow` on a NEW `LiveTvChannel`/`LiveTvProgram` item
-- and keeps it across refreshes. The channel/programme DTO paths project it as
-- the all-fields `DateCreated` a Jellyfin client sees on
-- `GET /LiveTv/Channels/{id}` and `GET /LiveTv/Programs/{id}`.
--
-- Ferrofin-owned tables (`FerrofinLiveTvChannels`/`FerrofinLiveTvPrograms`), so
-- the columns are additive and the Jellyfin-pinned schema shape is untouched.
ALTER TABLE "FerrofinLiveTvChannels" ADD COLUMN "DateCreated" TEXT;
ALTER TABLE "FerrofinLiveTvPrograms" ADD COLUMN "DateCreated" TEXT;
