-- Playback-decision metrics (brain/PLAN_PERFORMANCE.md, Track A).
--
-- One row per PlaybackInfo *decision*, keyed by the PlaySessionId the server
-- mints in the PlaybackInfo response (the client threads it through every
-- subsequent playstate report, which is what lets start/stop update the row).
-- The Transcode* cost columns are stamped by the transcode runtime when the
-- ffmpeg child exits (Phase A2); NULL until then / for direct play.
CREATE TABLE "PlaybackSessions" (
    "PlaySessionId" TEXT NOT NULL PRIMARY KEY,
    "ItemId" TEXT NOT NULL,
    "UserId" TEXT NOT NULL,
    "Client" TEXT NULL,
    "DeviceId" TEXT NULL,
    -- DirectPlay | DirectStream | Transcode (the final decision sent to the client)
    "PlayMethod" TEXT NOT NULL,
    -- Comma-separated TranscodeReason names; empty for direct play.
    "TranscodeReasons" TEXT NOT NULL DEFAULT '',
    "Container" TEXT NULL,
    "VideoCodec" TEXT NULL,
    "AudioCodec" TEXT NULL,
    "TargetContainer" TEXT NULL,
    "TargetVideoCodec" TEXT NULL,
    "TargetAudioCodec" TEXT NULL,
    "DecidedAt" TEXT NOT NULL,
    "StartedAt" TEXT NULL,
    "StoppedAt" TEXT NULL,
    "PositionTicks" INTEGER NULL,
    "TranscodeWallMs" INTEGER NULL,
    "TranscodeCpuMs" INTEGER NULL,
    "TranscodePeakRssKb" INTEGER NULL,
    "FirstSegmentMs" INTEGER NULL
);

CREATE INDEX "IX_PlaybackSessions_DecidedAt" ON "PlaybackSessions" ("DecidedAt");
