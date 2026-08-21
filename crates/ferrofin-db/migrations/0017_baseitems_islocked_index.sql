-- The scan reads the whole locked-item set once per run instead of hydrating a
-- row per item. Without an index that read is `SCAN BaseItems` — 26-53 ms warm
-- on a 100k-item / 317 MB table, and far worse cold.
--
-- That cost is fine amortized over a full scan, but `run_scan` is shared with
-- `scan_paths`, which the library monitor calls for a handful of items on every
-- filesystem event. A partial index keeps only the locked rows (normally very
-- few), so the read is 1-2 ms regardless of library size.
--
-- Ferrofin-owned, so it lives in the FerrofinIX_* namespace and leaves the
-- Jellyfin-pinned schema shape untouched (see the schema_conformance test).
CREATE INDEX IF NOT EXISTS "FerrofinIX_BaseItems_IsLocked"
    ON "BaseItems" ("IsLocked")
    WHERE "IsLocked" = 1;
