-- 0039: on a database adopted from Jellyfin 12.0 or 12.1, 0032 is baselined, so the
-- FerrofinIX_BaseItems_* copies 0009/0014 created survive next to Jellyfin's identical
-- IX_BaseItems_* set (fresh and 10.11 paths lose them in 0032's table rebuild). They
-- double the write cost and, once 12.2 makes the OwnerId pair partial, plan differently
-- from a fresh database. Never baselined: a no-op wherever they are already gone.
DROP INDEX IF EXISTS "FerrofinIX_BaseItems_ExtraType_OwnerId";
DROP INDEX IF EXISTS "FerrofinIX_BaseItems_Name";
DROP INDEX IF EXISTS "FerrofinIX_BaseItems_OwnerId";
DROP INDEX IF EXISTS "FerrofinIX_BaseItems_PrimaryVersionId";
DROP INDEX IF EXISTS "FerrofinIX_BaseItems_SeasonId";
DROP INDEX IF EXISTS "FerrofinIX_BaseItems_SeriesId";
DROP INDEX IF EXISTS "FerrofinIX_BaseItems_SeriesName";
DROP INDEX IF EXISTS "FerrofinIX_BaseItems_TopParentId_IsFolder_IsVirtualItem_DateCreated";
DROP INDEX IF EXISTS "FerrofinIX_BaseItems_TopParentId_MediaType_IsVirtualItem_DateCreated";
DROP INDEX IF EXISTS "FerrofinIX_BaseItems_TopParentId_Type_IsVirtualItem";
DROP INDEX IF EXISTS "FerrofinIX_BaseItems_TopParentId_Type_IsVirtualItem_DateCreated";
DROP INDEX IF EXISTS "FerrofinIX_BaseItems_Type_CleanName";
DROP INDEX IF EXISTS "FerrofinIX_BaseItems_Type_SeriesPresentationUniqueKey_ParentIndexNumber_IndexNumber";
DROP INDEX IF EXISTS "FerrofinIX_BaseItems_Type_TopParentId_SortName";
