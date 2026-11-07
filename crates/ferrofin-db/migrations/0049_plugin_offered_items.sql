-- 0049: add FerrofinPluginOfferedItems, the library items the plugin media-analysis pass has offered to each analyzer plugin.
--
-- The "Plugin media analysis" task offers each item of an analyzer plugin's scan targets to
-- it once. It used to remember how far it had got as a watermark, the newest DateCreated it
-- had offered, kept in the plugin's state file. DateCreated is not the order items arrive in
-- (it is a file's birth time, or the older of its ctime and mtime where there is none, a
-- photo's EXIF date, or what a user set), so an item brought in with old file times landed
-- behind the watermark and was never offered. The pass now records here each item it has
-- offered, and offers the items of its scan targets that have no row for the plugin.
--
-- One row per item and plugin, both stored as the BaseItems Id is (uppercase, hyphenated);
-- OfferedAt is when the item was offered. The key leads with ItemId, so the pass's per-item
-- probe and the cascade that removes a deleted item's rows are both key lookups; nothing else
-- is indexed, as only an uninstall reads by plugin alone, and it may scan. An item the
-- pass skipped (its library turns the plugin's segments off, the plugin's segment write was
-- refused, or the plugin's runtime refused the call) gets no row and is offered on a later
-- pass.
--
-- Purely additive: the table starts empty and the previous version never reads it. Each
-- plugin's first pass after the upgrade records every item at or before the plugin's old
-- watermark as offered, so the switch offers nothing twice, and it leaves the old watermark
-- where it is for a previous version still running beside this one.
CREATE TABLE "FerrofinPluginOfferedItems" (
    "PluginId" TEXT NOT NULL,
    "ItemId" TEXT NOT NULL REFERENCES "BaseItems" ("Id") ON DELETE CASCADE,
    "OfferedAt" TEXT NOT NULL,
    PRIMARY KEY ("ItemId", "PluginId")
) WITHOUT ROWID;
