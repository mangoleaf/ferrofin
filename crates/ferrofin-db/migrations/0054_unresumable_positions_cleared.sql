-- 0054: clear the resume positions builds before v1.4.0 saved for songs and for Sample, ThemeVideo and Trailer extras, which Jellyfin never lets resume.
--
-- Jellyfin keeps no position from a play of an item whose SupportsPositionTicksResume is
-- false (UserDataManager.cs:521, UpdatePlayState, v12.4): Audio keeps BaseItem's false
-- (MediaBrowser.Controller/Entities/BaseItem.cs:206), and a Video answers false for a
-- Sample, ThemeVideo or Trailer extra (MediaBrowser.Controller/Entities/Video.cs:63-88).
-- Ferrofin before v1.4.0 answered true for every song and every video extra (gl-46), so
-- such an item stopped midway kept its position. The resume query keys on the stored
-- position alone, as Jellyfin's does (BaseItemRepository.TranslateQuery.cs, IsResumable),
-- so those songs stayed in Continue Listening, and those extras in the resume rows, until
-- they were played again. The kinds are kinds.rs supports_position_ticks_resume's: Audio,
-- and the five video types with ExtraType 2 (Trailer), 7 (Sample) or 9 (ThemeVideo).
--
-- Only the position changes; Played, PlayCount, LastPlayedDate and the rest stay as stored,
-- on every CustomDataKey row of the item. A row detached onto the placeholder item (its
-- item deleted) keeps its position, since its kind went with the item; the 90-day purge
-- removes it.
--
-- Baselined for every Jellyfin generation (database.rs JELLYFIN_GENERATIONS), like 0010:
-- a Jellyfin database holds such a position only where a client set it through
-- POST /UserItems/{itemId}/UserData, which Jellyfin keeps (UserDataManager.cs:120-123),
-- so adoption leaves it as Jellyfin had it. A database Ferrofin already holds runs it; a
-- position a client set that way there since v1.4.0 cannot be told apart from a stale one
-- and is cleared once too.
--
-- The previous version, which can run beside this one during a rolling update, saves no
-- such position from a play: every release since v1.4.0 has the gl-46 fix. A no-op on a
-- fresh database.

UPDATE "UserData" SET "PlaybackPositionTicks" = 0
 WHERE "PlaybackPositionTicks" > 0
   AND "ItemId" IN (
       SELECT "Id" FROM "BaseItems"
        WHERE "Type" = 'MediaBrowser.Controller.Entities.Audio.Audio'
           OR ("ExtraType" IN (2, 7, 9)
               AND "Type" IN ('MediaBrowser.Controller.Entities.Video',
                              'MediaBrowser.Controller.Entities.Movies.Movie',
                              'MediaBrowser.Controller.Entities.TV.Episode',
                              'MediaBrowser.Controller.Entities.MusicVideo',
                              'MediaBrowser.Controller.Entities.Trailer')));
