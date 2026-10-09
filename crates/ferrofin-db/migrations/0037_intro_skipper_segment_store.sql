-- The Intro Skipper's own segment tier (the plugin's `DbSegment` and
-- `DbDisabledEpisode`). Detection and user edits land here; a publish step
-- copies what should be visible into Jellyfin's `MediaSegments`, as the
-- plugin's `SegmentProvider` does. Ferrofin-owned tables: Jellyfin keeps these
-- in the plugin's private database, which is not imported.
--
-- `ItemId`/`SeasonId`/`EpisodeId` are lowercase hyphenated GUIDs (the
-- `FerrofinIntroSkipperAnalyzerActions` convention); `Type` is the plugin's
-- `AnalysisMode` (Introduction 0, Credits 1, Preview 2, Recap 3, Commercial 4);
-- `Start`/`End` are seconds.
CREATE TABLE "FerrofinIntroSkipperSegments" (
    "Id" INTEGER PRIMARY KEY AUTOINCREMENT,
    "ItemId" TEXT NOT NULL,
    "Type" INTEGER NOT NULL,
    "Start" REAL NOT NULL,
    "End" REAL NOT NULL,
    "IsUserProvided" INTEGER NOT NULL DEFAULT 0,
    "ConfigHash" TEXT NOT NULL DEFAULT ''
);
CREATE INDEX "FerrofinIX_IntroSkipperSegments_ItemId_Type"
    ON "FerrofinIntroSkipperSegments" ("ItemId", "Type");

CREATE TABLE "FerrofinIntroSkipperDisabledEpisodes" (
    "SeasonId" TEXT NOT NULL,
    "EpisodeId" TEXT NOT NULL,
    PRIMARY KEY ("SeasonId", "EpisodeId")
);
CREATE INDEX "FerrofinIX_IntroSkipperDisabledEpisodes_EpisodeId"
    ON "FerrofinIntroSkipperDisabledEpisodes" ("EpisodeId");

-- Seed the tier from the segments already published, so the first publish
-- after upgrade does not wipe them: Ferrofin's own (provider `IntroSkipper`)
-- and, on an adopted Jellyfin database, the plugin's (its provider id, the
-- MD5 of `intro skipper` that Jellyfin's `MediaSegmentManager.GetProviderId`
-- derives). `MediaSegmentType` → `AnalysisMode`: Intro 5 → 0, Outro 4 → 1,
-- Preview 2 → 2, Recap 3 → 3, Commercial 1 → 4; ticks → seconds.
-- Every seeded row is an analysis result (`IsUserProvided` 0): the old
-- direct writes never recorded who made them, so a timestamp a user edited
-- through Ferrofin before this tier existed can be replaced once by the next
-- analysis, after which edits are protected.
INSERT INTO "FerrofinIntroSkipperSegments" ("ItemId", "Type", "Start", "End")
SELECT lower("ItemId"),
       CASE "Type" WHEN 5 THEN 0 WHEN 4 THEN 1 WHEN 2 THEN 2 WHEN 3 THEN 3 WHEN 1 THEN 4 END,
       "StartTicks" / 10000000.0,
       "EndTicks" / 10000000.0
FROM "MediaSegments"
WHERE "SegmentProviderId" IN ('IntroSkipper', 'b0338b450421c081992860f1d02f261f')
  AND "Type" IN (1, 2, 3, 4, 5);

-- Jellyfin's provider id for the plugin, so a re-publish replaces the rows an
-- adopted database already holds instead of duplicating them.
UPDATE "MediaSegments"
SET "SegmentProviderId" = 'b0338b450421c081992860f1d02f261f'
WHERE "SegmentProviderId" = 'IntroSkipper';
