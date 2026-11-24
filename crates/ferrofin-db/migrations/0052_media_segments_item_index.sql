-- 0052: an index on MediaSegments' ItemId and StartTicks for the per-item segment statements:
-- PlaybackInfo's HasSegments count and GET /MediaSegments/{itemId}'s read at every playback
-- start, a provider's replace, an item's delete and re-key. With only the primary key on Id,
-- each one scanned the whole table (and the read sorted on StartTicks), which grows with every
-- analysed episode. Ferrofin-own: Jellyfin has no index on the table and never reads this one.
CREATE INDEX IF NOT EXISTS "FerrofinIX_MediaSegments_ItemId_StartTicks"
    ON "MediaSegments" ("ItemId", "StartTicks");
