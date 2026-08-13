-- Index BaseItems.PrimaryVersionId (Ferrofin-own, Hermit namespace).
--
-- The user-data ORDER BY expressions (DatePlayed/PlayCount sorts) and the
-- merged-version lookups correlate per row over
--   oud."ItemId" IN (SELECT alt."Id" FROM "BaseItems" alt
--                    WHERE alt."PrimaryVersionId" = bi."Id")
-- Without an index that inner SELECT is a full BaseItems scan PER SORTED ROW —
-- observed live as a sort request that never returns on an 8.7k-episode
-- library. Partial (NOT NULL) keeps it tiny: only merged alternates carry the
-- column.
CREATE INDEX IF NOT EXISTS "HermitIX_BaseItems_PrimaryVersionId"
    ON "BaseItems" ("PrimaryVersionId")
    WHERE "PrimaryVersionId" IS NOT NULL;
