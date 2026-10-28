-- 0043: by-name rows are not folders. Jellyfin stores the runtime `BaseItem.IsFolder`
-- (Jellyfin.Server.Implementations/Item/BaseItemMapper.cs:415-419, v12.2): `Genre`,
-- `MusicGenre`, `Studio`, `Person` and `Year` derive from `BaseItem`, so they store 0, and
-- a `MusicArtist` with no parent is accessed by name, whose `IsFolder => !IsAccessedByName`
-- (MediaBrowser.Controller/Entities/Audio/MusicArtist.cs:30-33) is false. Earlier Ferrofin
-- scans stored 1 on every by-name `MusicArtist` row, and earlier builds on genre and studio
-- rows too. The played filter (`BuildIsPlayedFilter`, which rolls a folder up from the
-- leaves below it) then read every such row as played, and search hints marked them as
-- folders. A physically parented artist folder keeps 1, as upstream. "No parent" is a NULL
-- "ParentId": the foreign key rules out the empty GUID, which names no row.
--
-- Never baselined: a no-op on a Jellyfin database, which already stores 0.
UPDATE "BaseItems" SET "IsFolder" = 0
WHERE "IsFolder" <> 0
  AND ("Type" IN ('MediaBrowser.Controller.Entities.Genre',
                  'MediaBrowser.Controller.Entities.Audio.MusicGenre',
                  'MediaBrowser.Controller.Entities.Studio',
                  'MediaBrowser.Controller.Entities.Person',
                  'MediaBrowser.Controller.Entities.Year')
       OR ("Type" = 'MediaBrowser.Controller.Entities.Audio.MusicArtist' AND "ParentId" IS NULL));
