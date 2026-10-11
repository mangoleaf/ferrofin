-- The Intro Skipper's full per-season state (the plugin's `DbSeasonState`):
-- besides the analyzer action 0036 stored, the episodes each mode has
-- analysed (`EpisodeIds`, so an episode without a result is not
-- re-fingerprinted every run), the configuration hash they were analysed
-- under (`ConfigHash`, so a settings change re-analyses), and the episodes a
-- settled-season reanalysis last covered (`SettledReanalysisEpisodeIds`).
-- The id lists are JSON arrays of lowercase hyphenated GUIDs; `Type` is the
-- plugin's `AnalysisMode`.
CREATE TABLE "FerrofinIntroSkipperSeasonStates" (
    "SeasonId" TEXT NOT NULL,
    "Type" INTEGER NOT NULL,
    "Action" INTEGER NOT NULL DEFAULT 0,
    "EpisodeIds" TEXT NOT NULL DEFAULT '[]',
    "ConfigHash" TEXT NOT NULL DEFAULT '',
    "SettledReanalysisEpisodeIds" TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY ("SeasonId", "Type")
);

INSERT INTO "FerrofinIntroSkipperSeasonStates" ("SeasonId", "Type", "Action")
SELECT "SeasonId", "Mode", "Action" FROM "FerrofinIntroSkipperAnalyzerActions";

DROP TABLE "FerrofinIntroSkipperAnalyzerActions";
