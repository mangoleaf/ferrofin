-- The Intro Skipper's per-season analyzer actions (the plugin's
-- `DbSeasonState.Action`, set by `POST /Intros/AnalyzerActions/UpdateSeason`
-- and honoured by detection). A Ferrofin-owned table: Jellyfin keeps this in
-- the plugin's own database. `Mode`/`Action` are the plugin's enum values.
CREATE TABLE "FerrofinIntroSkipperAnalyzerActions" (
    "SeasonId" TEXT NOT NULL,
    "Mode" INTEGER NOT NULL,
    "Action" INTEGER NOT NULL,
    PRIMARY KEY ("SeasonId", "Mode")
);
