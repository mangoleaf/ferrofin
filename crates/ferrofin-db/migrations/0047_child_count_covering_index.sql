-- 0047: a covering index for GetChildCountBatch's direct-children count, which groups a
-- page's folders' children on ParentId. When it was added the count also read each
-- child's SeasonId, IsVirtualItem, OwnerId, ExtraType, PrimaryVersionId and TopParentId
-- (the last for the alternate-version check), so on IX_BaseItems_ParentId every child
-- was a table read; /Users/{id}/Views paid 1.0 ms for three libraries of ~3,500
-- children. Ferrofin-own: Jellyfin has no such index and never reads it.
CREATE INDEX IF NOT EXISTS "FerrofinIX_BaseItems_ChildCount"
    ON "BaseItems" ("ParentId", "SeasonId", "IsVirtualItem", "OwnerId", "ExtraType", "PrimaryVersionId", "TopParentId");
