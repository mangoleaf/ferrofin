-- Covering expression index for the /Persons dedup-by-name aggregate.
--
-- The by-name people listing collapses the Peoples table to one representative
-- row per LOWER(Name) (MIN(Id) — upstream's distinct-names behavior). Without
-- this index that aggregate is a full table scan + temp b-tree PER REQUEST
-- (28 ms on a 7.5k-person library through sqlx), which is the core of the
-- benchmark finding where /Persons collapsed to a 29.8 s p50 at its calibrated
-- 608 req/s. With it the aggregate is an index-only scan: 0.85 ms measured on
-- the same data, byte-identical results.
--
-- Hermit* namespace: Ferrofin-own additive object, invisible to the pinned
-- Jellyfin 10.11.8 schema (drop-in adoption stays two-way safe).
CREATE INDEX IF NOT EXISTS "HermitIX_Peoples_LowerName_Cover"
    ON "Peoples" (LOWER("Name"), "Name", "PersonType", "Id");
