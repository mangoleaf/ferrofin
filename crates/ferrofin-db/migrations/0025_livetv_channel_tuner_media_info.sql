-- A Live TV channel carries the per-channel media facts its tuner's lineup
-- reported, so the media source a client is offered can be built from them.
--
-- `HdHomerunHost.GetMediaSource` (v10.11.8
-- src/Jellyfin.LiveTv/TunerHosts/HdHomerun/HdHomerunHost.cs) reads three fields
-- off the `ChannelInfo` the lineup produced — `IsHD`, `VideoCodec` and
-- `AudioCodec` — and they decide the source's resolution, its video/audio
-- bitrates and the `NalLengthSize` marker. They come from `lineup.json`
-- (`{"GuideNumber":"4.1","HD":1,...}`), i.e. from the tuner, not from probing,
-- so they have to survive the refresh that fetched them rather than being
-- re-derived at playback time from a lineup that may have moved on.
--
-- `IsHD` is a nullable flag on purpose: upstream's `ChannelInfo.IsHD` is
-- `bool?` and `GetMediaSource` reads it as `channelInfo.IsHD ?? true`, so
-- "the tuner did not say" and "the tuner said no" are different answers.
--
-- Ferrofin-owned table (`FerrofinLiveTvChannels`), so this is additive and the
-- Jellyfin-pinned schema shape is untouched. Existing rows get NULL — the same
-- "the tuner did not say" the M3U host reports, which is what it reported
-- before this column existed.
ALTER TABLE "FerrofinLiveTvChannels" ADD COLUMN "IsHd" INTEGER;
ALTER TABLE "FerrofinLiveTvChannels" ADD COLUMN "VideoCodec" TEXT;
ALTER TABLE "FerrofinLiveTvChannels" ADD COLUMN "AudioCodec" TEXT;
