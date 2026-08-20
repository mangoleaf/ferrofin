#!/usr/bin/env python3
"""Benchmark coverage of the vendored contract — the "no silent omissions" gate.

`suite/registry.json` says which contract operations the benchmark *measures*.
This module says why every remaining one is **not** measured. Together they must
partition the vendored spec exactly:

    measured (suite/registry.json)  ∪  skipped (SKIPPED below)  ==  every operation

Run it (exit 0 = green, and it prints the split):

    python3 suite/coverage.py

The point is that a contract operation can never fall out of the benchmark
quietly. Adding a path to the vendored spec — or deleting a bench row — breaks
this check until someone either measures the operation or writes down a reason
here. A reason is a slug from :data:`SKIP_REASONS`; the prose lives there once
so the per-operation table stays scannable.

Adding a bench row is always preferable to adding a skip. The bar for a row is
the one `endpoints.py` documents: a stateless request whose params resolve from
`benchlib.enrich_context`, deterministic output, and 2xx on BOTH servers over
the bench fixture (movies + episodes, no music, no tuner, no uploaded assets).
"""
import ast
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SPEC = next((ROOT / "contracts").glob("jellyfin-openapi-*.json"))
REGISTRY = Path(__file__).resolve().parent / "registry.json"

METHODS = ("get", "post", "put", "delete", "patch", "head", "options")

#: Why an operation is not benched. One entry per slug used in :data:`SKIPPED`.
SKIP_REASONS = {
    "destructive": (
        "mutates or removes library/user/config state and does not undo itself, so a "
        "measured window would leave the fixture somewhere else than it found it"),
    "config-write": (
        "rewrites server configuration; the next row in the same run would then be "
        "measuring a differently-configured server"),
    "lifecycle": (
        "restarts, shuts down, or re-runs first-boot on the server — it ends the run "
        "rather than taking part in it"),
    "scan": (
        "kicks a library scan or scheduled task; the work outlives the 10s window and "
        "poisons every measurement after it"),
    "write-fixture": (
        "reads state that only a write can create (a backup, a QuickConnect secret, a "
        "SyncPlay group, a running encode) and no read-only fixture exists"),
    "fast-loop": (
        "state-preserving write measured by the fast loop only "
        "(suite/micro/write_endpoints.py) — deliberately out of the gate's table so "
        "suite/perf-baseline.json stays comparable across releases"),
    "session-write": (
        "mutates session/playstate bookkeeping; the gate already carries "
        "playstate_progress as the representative row for that path"),
    "syncplay-write": (
        "SyncPlay group command — needs a joined group and leaves the group in a new "
        "state, so it is a journey (suite/parity), not a repeatable load row"),
    "streaming": (
        "media byte stream (direct play, HLS playlist/segment, live stream): it "
        "measures ffmpeg, disk and socket throughput, not the server's request path"),
    "asset": (
        "binary asset that must already exist on disk (image, font, splashscreen, "
        "trickplay tile, attachment, subtitle file); the bench fixture ships none"),
    "upload": (
        "accepts a binary/multipart upload — the row would measure the load generator "
        "pushing bytes"),
    "network": (
        "reaches an external service (TMDB, OpenSubtitles, SchedulesDirect, the plugin "
        "repository): non-deterministic, and unfair between the two legs because only "
        "one of them has the provider enabled"),
    "extension": (
        "Ferrofin extension / plugin route with no counterpart on the stock Jellyfin "
        "leg, so the comparison would be Ferrofin against a 404"),
    "music": (
        "needs an audio library; the bench fixture is movies + episodes, so the row "
        "would measure an empty-result path under a music-shaped name"),
    "livetv-fixture": (
        "needs a Live TV tuner/DVR fixture (channel, program, recording, timer); the "
        "bench fixture configures no tuner"),
    "channel-fixture": (
        "needs a channel provider (an IPTV/plugin channel); the bench fixture has none"),
    "host-fs": (
        "the answer is a listing of the container's own filesystem, which differs "
        "between the Ferrofin and Jellyfin images — not a comparable measurement"),
    "bandwidth": (
        "returns a synthetic N-byte payload on request: the row would measure the "
        "loopback interface, and at bench rates it would starve every other row"),
    "divergence": (
        "Ferrofin answers 404 by design (accepted divergence, suite/parity/LEDGER.md) "
        "— there is no successful response to measure"),
}

#: Every contract operation the benchmark does NOT measure → its reason slug.
SKIPPED = {
    # ── asset (36)
    "DELETE /Branding/Splashscreen": "asset",
    "DELETE /Items/{itemId}/Images/{imageType}": "asset",
    "DELETE /Items/{itemId}/Images/{imageType}/{imageIndex}": "asset",
    "DELETE /UserImage": "asset",
    "DELETE /Videos/{itemId}/Subtitles/{index}": "asset",
    "GET /Artists/{name}/Images/{imageType}/{imageIndex}": "asset",
    "GET /Branding/Splashscreen": "asset",
    "GET /FallbackFont/Fonts/{name}": "asset",
    "GET /Genres/{name}/Images/{imageType}": "asset",
    "GET /Genres/{name}/Images/{imageType}/{imageIndex}": "asset",
    "GET /MusicGenres/{name}/Images/{imageType}": "asset",
    "GET /MusicGenres/{name}/Images/{imageType}/{imageIndex}": "asset",
    "GET /Persons/{name}/Images/{imageType}": "asset",
    "GET /Persons/{name}/Images/{imageType}/{imageIndex}": "asset",
    "GET /Studios/{name}/Images/{imageType}": "asset",
    "GET /Studios/{name}/Images/{imageType}/{imageIndex}": "asset",
    "GET /UserImage": "asset",
    "GET /Videos/{itemId}/Trickplay/{width}/tiles.m3u8": "asset",
    "GET /Videos/{itemId}/Trickplay/{width}/{index}.jpg": "asset",
    "GET /Videos/{itemId}/{mediaSourceId}/Subtitles/{index}/subtitles.m3u8": "asset",
    "GET /Videos/{routeItemId}/{routeMediaSourceId}/Subtitles/{routeIndex}/Stream.{routeFormat}": "asset",
    "GET /Videos/{routeItemId}/{routeMediaSourceId}/Subtitles/{routeIndex}/{routeStartPositionTicks}/Stream.{routeFormat}": "asset",
    "GET /Videos/{videoId}/{mediaSourceId}/Attachments/{index}": "asset",
    "HEAD /Artists/{name}/Images/{imageType}/{imageIndex}": "asset",
    "HEAD /Genres/{name}/Images/{imageType}": "asset",
    "HEAD /Genres/{name}/Images/{imageType}/{imageIndex}": "asset",
    "HEAD /Items/{itemId}/Images/{imageType}": "asset",
    "HEAD /Items/{itemId}/Images/{imageType}/{imageIndex}": "asset",
    "HEAD /Items/{itemId}/Images/{imageType}/{imageIndex}/{tag}/{format}/{maxWidth}/{maxHeight}/{percentPlayed}/{unplayedCount}": "asset",
    "HEAD /MusicGenres/{name}/Images/{imageType}": "asset",
    "HEAD /MusicGenres/{name}/Images/{imageType}/{imageIndex}": "asset",
    "HEAD /Persons/{name}/Images/{imageType}": "asset",
    "HEAD /Persons/{name}/Images/{imageType}/{imageIndex}": "asset",
    "HEAD /Studios/{name}/Images/{imageType}": "asset",
    "HEAD /Studios/{name}/Images/{imageType}/{imageIndex}": "asset",
    "HEAD /UserImage": "asset",

    # ── bandwidth (1)
    "GET /Playback/BitrateTest": "bandwidth",

    # ── channel-fixture (2)
    "GET /Channels/{channelId}/Features": "channel-fixture",
    "GET /Channels/{channelId}/Items": "channel-fixture",

    # ── config-write (8)
    "POST /Devices/Options": "config-write",
    "POST /Library/VirtualFolders/LibraryOptions": "config-write",
    "POST /Repositories": "config-write",
    "POST /System/Configuration": "config-write",
    "POST /System/Configuration/Branding": "config-write",
    "POST /System/Configuration/{key}": "config-write",
    "POST /Users/Configuration": "config-write",
    "POST /Users/{userId}/Policy": "config-write",

    # ── destructive (32)
    "DELETE /Auth/Keys/{key}": "destructive",
    "DELETE /Collections/{collectionId}/Items": "destructive",
    "DELETE /Devices": "destructive",
    "DELETE /Items": "destructive",
    "DELETE /Items/{itemId}": "destructive",
    "DELETE /Library/VirtualFolders": "destructive",
    "DELETE /Library/VirtualFolders/Paths": "destructive",
    "DELETE /Playlists/{playlistId}/Items": "destructive",
    "DELETE /Playlists/{playlistId}/Users/{userId}": "destructive",
    "DELETE /UserFavoriteItems/{itemId}": "destructive",
    "DELETE /UserItems/{itemId}/Rating": "destructive",
    "DELETE /UserPlayedItems/{itemId}": "destructive",
    "DELETE /Users/{userId}": "destructive",
    "DELETE /Videos/{itemId}/AlternateSources": "destructive",
    "POST /Auth/Keys": "destructive",
    "POST /Collections": "destructive",
    "POST /Collections/{collectionId}/Items": "destructive",
    "POST /Items/{itemId}": "destructive",
    "POST /Items/{itemId}/ContentType": "destructive",
    "POST /Items/{itemId}/Images/{imageType}/{imageIndex}/Index": "destructive",
    "POST /Library/VirtualFolders": "destructive",
    "POST /Library/VirtualFolders/Name": "destructive",
    "POST /Library/VirtualFolders/Paths": "destructive",
    "POST /Library/VirtualFolders/Paths/Update": "destructive",
    "POST /Playlists": "destructive",
    "POST /Playlists/{playlistId}": "destructive",
    "POST /Playlists/{playlistId}/Users/{userId}": "destructive",
    "POST /ScheduledTasks/{taskId}/Triggers": "destructive",
    "POST /Users": "destructive",
    "POST /Users/New": "destructive",
    "POST /Users/Password": "destructive",
    "POST /Videos/MergeVersions": "destructive",

    # ── divergence (2)
    "GET /Items/Root": "divergence",
    "GET /Years/{year}": "divergence",

    # ── extension (33)
    "DELETE /Intros/Show/{SeriesId}/{SeasonId}": "extension",
    "DELETE /MediaSegmentsApi/{segmentId}": "extension",
    "DELETE /Plugins/{pluginId}": "extension",
    "DELETE /Plugins/{pluginId}/{version}": "extension",
    "GET /Episode/{Id}/Timestamps": "extension",
    "GET /Episode/{id}/IntroSkipperSegments": "extension",
    "GET /IntroSkipper": "extension",
    "GET /IntroSkipper/SupportBundle": "extension",
    "GET /Intros/AnalyzerActions/{SeasonId}": "extension",
    "GET /Intros/ScanStatus": "extension",
    "GET /Intros/Show/{SeriesId}/{SeasonId}": "extension",
    "GET /MediaSegmentsApi": "extension",
    "GET /Plugins/{pluginId}/Configuration": "extension",
    "GET /Plugins/{pluginId}/{version}/Image": "extension",
    "GET /web/ConfigurationPage": "extension",
    "POST /Episode/{Id}/Timestamps": "extension",
    "POST /FileTransformation/RegisterTransformation": "extension",
    "POST /Intros/AnalyzerActions/UpdateSeason": "extension",
    "POST /Intros/EraseTimestamps": "extension",
    "POST /Intros/RebuildDatabase": "extension",
    "POST /Intros/ScanSeason/{SeriesId}/{SeasonId}": "extension",
    "POST /Jellyfin.Plugin.OpenSubtitles/ValidateLoginInfo": "extension",
    "POST /MediaSegmentsApi/{itemId}": "extension",
    "POST /MergeVersions/MergeEpisodes": "extension",
    "POST /MergeVersions/MergeMovies": "extension",
    "POST /MergeVersions/SplitEpisodes": "extension",
    "POST /MergeVersions/SplitMovies": "extension",
    "POST /Plugins/{pluginId}/Configuration": "extension",
    "POST /Plugins/{pluginId}/Manifest": "extension",
    "POST /Plugins/{pluginId}/{version}/Disable": "extension",
    "POST /Plugins/{pluginId}/{version}/Enable": "extension",
    "POST /SkipButtonCss/InjectCss": "extension",
    "POST /SkipButtonCss/UpdateSkipDuration": "extension",

    # ── fast-loop (11)
    "POST /DisplayPreferences/{displayPreferencesId}": "fast-loop",
    "POST /PlayingItems/{itemId}": "fast-loop",
    "POST /PlayingItems/{itemId}/Progress": "fast-loop",
    "POST /Playlists/{playlistId}/Items": "fast-loop",
    "POST /Playlists/{playlistId}/Items/{itemId}/Move/{newIndex}": "fast-loop",
    "POST /Sessions/Playing": "fast-loop",
    "POST /Sessions/Playing/Ping": "fast-loop",
    "POST /Sessions/Playing/Stopped": "fast-loop",
    "POST /UserFavoriteItems/{itemId}": "fast-loop",
    "POST /UserItems/{itemId}/Rating": "fast-loop",
    "POST /UserPlayedItems/{itemId}": "fast-loop",

    # ── host-fs (3)
    "GET /Environment/DirectoryContents": "host-fs",
    "GET /System/Logs/Log": "host-fs",
    "POST /Environment/ValidatePath": "host-fs",

    # ── lifecycle (8)
    "POST /Backup/Create": "lifecycle",
    "POST /Backup/Restore": "lifecycle",
    "POST /Startup/Complete": "lifecycle",
    "POST /Startup/Configuration": "lifecycle",
    "POST /Startup/RemoteAccess": "lifecycle",
    "POST /Startup/User": "lifecycle",
    "POST /System/Restart": "lifecycle",
    "POST /System/Shutdown": "lifecycle",

    # ── livetv-fixture (19)
    "DELETE /LiveTv/ListingProviders": "livetv-fixture",
    "DELETE /LiveTv/Recordings/{recordingId}": "livetv-fixture",
    "DELETE /LiveTv/SeriesTimers/{timerId}": "livetv-fixture",
    "DELETE /LiveTv/Timers/{timerId}": "livetv-fixture",
    "DELETE /LiveTv/TunerHosts": "livetv-fixture",
    "GET /LiveTv/Channels/{channelId}": "livetv-fixture",
    "GET /LiveTv/Programs/{programId}": "livetv-fixture",
    "GET /LiveTv/Recordings/Groups/{groupId}": "livetv-fixture",
    "GET /LiveTv/Recordings/{recordingId}": "livetv-fixture",
    "GET /LiveTv/SeriesTimers/{timerId}": "livetv-fixture",
    "GET /LiveTv/Timers/{timerId}": "livetv-fixture",
    "POST /LiveTv/ChannelMappings": "livetv-fixture",
    "POST /LiveTv/ListingProviders": "livetv-fixture",
    "POST /LiveTv/SeriesTimers": "livetv-fixture",
    "POST /LiveTv/SeriesTimers/{timerId}": "livetv-fixture",
    "POST /LiveTv/Timers": "livetv-fixture",
    "POST /LiveTv/Timers/{timerId}": "livetv-fixture",
    "POST /LiveTv/TunerHosts": "livetv-fixture",
    "POST /LiveTv/Tuners/{tunerId}/Reset": "livetv-fixture",

    # ── music (13)
    "DELETE /Audio/{itemId}/Lyrics": "music",
    "GET /Albums/{itemId}/InstantMix": "music",
    "GET /Albums/{itemId}/Similar": "music",
    "GET /Artists/InstantMix": "music",
    "GET /Artists/{itemId}/InstantMix": "music",
    "GET /Artists/{itemId}/Similar": "music",
    "GET /Artists/{name}": "music",
    "GET /Audio/{itemId}/Lyrics": "music",
    "GET /MusicGenres/InstantMix": "music",
    "GET /MusicGenres/{genreName}": "music",
    "GET /MusicGenres/{name}/InstantMix": "music",
    "GET /Songs/{itemId}/InstantMix": "music",
    "POST /Audio/{itemId}/Lyrics": "music",

    # ── network (25)
    "DELETE /Packages/Installing/{packageId}": "network",
    "GET /Audio/{itemId}/RemoteSearch/Lyrics": "network",
    "GET /Items/{itemId}/RemoteImages": "network",
    "GET /Items/{itemId}/RemoteSearch/Subtitles/{language}": "network",
    "GET /LiveTv/ListingProviders/SchedulesDirect/Countries": "network",
    "GET /LiveTv/Tuners/Discover": "network",
    "GET /LiveTv/Tuners/Discvover": "network",
    "GET /Packages": "network",
    "GET /Packages/{name}": "network",
    "GET /Providers/Lyrics/{lyricId}": "network",
    "GET /Providers/Subtitles/Subtitles/{subtitleId}": "network",
    "POST /Audio/{itemId}/RemoteSearch/Lyrics/{lyricId}": "network",
    "POST /Items/RemoteSearch/Apply/{itemId}": "network",
    "POST /Items/RemoteSearch/Book": "network",
    "POST /Items/RemoteSearch/BoxSet": "network",
    "POST /Items/RemoteSearch/Movie": "network",
    "POST /Items/RemoteSearch/MusicAlbum": "network",
    "POST /Items/RemoteSearch/MusicArtist": "network",
    "POST /Items/RemoteSearch/MusicVideo": "network",
    "POST /Items/RemoteSearch/Person": "network",
    "POST /Items/RemoteSearch/Series": "network",
    "POST /Items/RemoteSearch/Trailer": "network",
    "POST /Items/{itemId}/RemoteImages/Download": "network",
    "POST /Items/{itemId}/RemoteSearch/Subtitles/{subtitleId}": "network",
    "POST /Packages/Installed/{name}": "network",

    # ── scan (8)
    "POST /Items/{itemId}/Refresh": "scan",
    "POST /Library/Media/Updated": "scan",
    "POST /Library/Movies/Added": "scan",
    "POST /Library/Movies/Updated": "scan",
    "POST /Library/Refresh": "scan",
    "POST /Library/Series/Added": "scan",
    "POST /Library/Series/Updated": "scan",
    "POST /ScheduledTasks/Running/{taskId}": "scan",

    # ── session-write (16)
    "DELETE /PlayingItems/{itemId}": "session-write",
    "DELETE /Sessions/{sessionId}/User/{userId}": "session-write",
    "POST /ClientLog/Document": "session-write",
    "POST /Sessions/Capabilities": "session-write",
    "POST /Sessions/Capabilities/Full": "session-write",
    "POST /Sessions/Logout": "session-write",
    "POST /Sessions/Viewing": "session-write",
    "POST /Sessions/{sessionId}/Command": "session-write",
    "POST /Sessions/{sessionId}/Command/{command}": "session-write",
    "POST /Sessions/{sessionId}/Message": "session-write",
    "POST /Sessions/{sessionId}/Playing": "session-write",
    "POST /Sessions/{sessionId}/Playing/{command}": "session-write",
    "POST /Sessions/{sessionId}/System/{command}": "session-write",
    "POST /Sessions/{sessionId}/User/{userId}": "session-write",
    "POST /Sessions/{sessionId}/Viewing": "session-write",
    "POST /System/Ping": "session-write",

    # ── streaming (29)
    "GET /Audio/{itemId}/hls/{segmentId}/stream.aac": "streaming",
    "GET /Audio/{itemId}/hls/{segmentId}/stream.mp3": "streaming",
    "GET /Audio/{itemId}/hls1/{playlistId}/{segmentId}.{container}": "streaming",
    "GET /Audio/{itemId}/main.m3u8": "streaming",
    "GET /Audio/{itemId}/master.m3u8": "streaming",
    "GET /Audio/{itemId}/stream": "streaming",
    "GET /Audio/{itemId}/stream.{container}": "streaming",
    "GET /Audio/{itemId}/universal": "streaming",
    "GET /Items/{itemId}/Download": "streaming",
    "GET /Items/{itemId}/File": "streaming",
    "GET /LiveTv/LiveRecordings/{recordingId}/stream": "streaming",
    "GET /LiveTv/LiveStreamFiles/{streamId}/stream.{container}": "streaming",
    "GET /Videos/{itemId}/hls/{playlistId}/stream.m3u8": "streaming",
    "GET /Videos/{itemId}/hls/{playlistId}/{segmentId}.{segmentContainer}": "streaming",
    "GET /Videos/{itemId}/hls1/{playlistId}/{segmentId}.{container}": "streaming",
    "GET /Videos/{itemId}/live.m3u8": "streaming",
    "GET /Videos/{itemId}/main.m3u8": "streaming",
    "GET /Videos/{itemId}/master.m3u8": "streaming",
    "GET /Videos/{itemId}/stream": "streaming",
    "GET /Videos/{itemId}/stream.{container}": "streaming",
    "HEAD /Audio/{itemId}/master.m3u8": "streaming",
    "HEAD /Audio/{itemId}/stream": "streaming",
    "HEAD /Audio/{itemId}/stream.{container}": "streaming",
    "HEAD /Audio/{itemId}/universal": "streaming",
    "HEAD /Videos/{itemId}/master.m3u8": "streaming",
    "HEAD /Videos/{itemId}/stream": "streaming",
    "HEAD /Videos/{itemId}/stream.{container}": "streaming",
    "POST /LiveStreams/Close": "streaming",
    "POST /LiveStreams/Open": "streaming",

    # ── syncplay-write (20)
    "POST /SyncPlay/Buffering": "syncplay-write",
    "POST /SyncPlay/Join": "syncplay-write",
    "POST /SyncPlay/Leave": "syncplay-write",
    "POST /SyncPlay/MovePlaylistItem": "syncplay-write",
    "POST /SyncPlay/New": "syncplay-write",
    "POST /SyncPlay/NextItem": "syncplay-write",
    "POST /SyncPlay/Pause": "syncplay-write",
    "POST /SyncPlay/Ping": "syncplay-write",
    "POST /SyncPlay/PreviousItem": "syncplay-write",
    "POST /SyncPlay/Queue": "syncplay-write",
    "POST /SyncPlay/Ready": "syncplay-write",
    "POST /SyncPlay/RemoveFromPlaylist": "syncplay-write",
    "POST /SyncPlay/Seek": "syncplay-write",
    "POST /SyncPlay/SetIgnoreWait": "syncplay-write",
    "POST /SyncPlay/SetNewQueue": "syncplay-write",
    "POST /SyncPlay/SetPlaylistItem": "syncplay-write",
    "POST /SyncPlay/SetRepeatMode": "syncplay-write",
    "POST /SyncPlay/SetShuffleMode": "syncplay-write",
    "POST /SyncPlay/Stop": "syncplay-write",
    "POST /SyncPlay/Unpause": "syncplay-write",

    # ── upload (5)
    "POST /Branding/Splashscreen": "upload",
    "POST /Items/{itemId}/Images/{imageType}": "upload",
    "POST /Items/{itemId}/Images/{imageType}/{imageIndex}": "upload",
    "POST /UserImage": "upload",
    "POST /Videos/{itemId}/Subtitles": "upload",

    # ── write-fixture (11)
    "DELETE /ScheduledTasks/Running/{taskId}": "write-fixture",
    "DELETE /Videos/ActiveEncodings": "write-fixture",
    "GET /Backup/Manifest": "write-fixture",
    "GET /Devices/Options": "write-fixture",
    "GET /QuickConnect/Connect": "write-fixture",
    "GET /SyncPlay/{id}": "write-fixture",
    "POST /QuickConnect/Authorize": "write-fixture",
    "POST /QuickConnect/Initiate": "write-fixture",
    "POST /Users/AuthenticateWithQuickConnect": "write-fixture",
    "POST /Users/ForgotPassword": "write-fixture",
    "POST /Users/ForgotPassword/Pin": "write-fixture",
}


def spec_operations():
    """Every operation in the vendored contract as "METHOD /path"."""
    paths = json.loads(SPEC.read_text())["paths"]
    return {f"{method.upper()} {path}"
            for path, item in paths.items()
            for method in item if method in METHODS}


def measured_operations():
    """Every operation suite/registry.json carries at least one bench variant for."""
    return {entry["op"] for entry in json.loads(REGISTRY.read_text())["operations"]}


def duplicate_skip_keys():
    """Operations written twice in the SKIPPED literal.

    Python keeps the last of two identical dict keys and says nothing, so a
    second entry with a *different* reason would silently win — exactly the
    quiet drift this module exists to prevent. Read the literal back from the
    source to catch it."""
    tree = ast.parse(Path(__file__).read_text())
    for node in ast.walk(tree):
        if isinstance(node, ast.Assign) and getattr(node.targets[0], "id", "") == "SKIPPED":
            keys = [k.value for k in node.value.keys]
            return sorted({k for k in keys if keys.count(k) > 1})
    return []


def audit():
    """(measured, skipped, unaccounted, bogus_skips, bad_reasons) over the spec."""
    ops = spec_operations()
    measured = measured_operations() & ops
    skipped = {op for op in SKIPPED if op in ops}
    return (measured,
            skipped,
            ops - measured - skipped,
            set(SKIPPED) - ops,
            {op for op, r in SKIPPED.items() if r not in SKIP_REASONS})


def main():
    measured, skipped, unaccounted, bogus, bad_reasons = audit()
    total = len(spec_operations())
    errors = []
    for op in sorted(unaccounted):
        errors.append(f"{op} is neither benched nor skipped — add a bench row to "
                      f"suite/perf/endpoints.py or an entry to SKIPPED")
    for op in sorted(bogus):
        errors.append(f"{op} is in SKIPPED but not in the vendored spec — stale entry")
    for op in sorted(bad_reasons):
        errors.append(f"{op} has reason {SKIPPED[op]!r}, which is not in SKIP_REASONS")
    for op in sorted(measured & set(SKIPPED)):
        errors.append(f"{op} is both benched and skipped — remove the SKIPPED entry")
    for op in duplicate_skip_keys():
        errors.append(f"{op} appears twice in SKIPPED — the second reason silently wins")

    if errors:
        print("benchmark coverage FAILED:", file=sys.stderr)
        for e in errors:
            print(f"  - {e}", file=sys.stderr)
        sys.exit(1)

    by_reason = {}
    for op in skipped:
        by_reason[SKIPPED[op]] = by_reason.get(SKIPPED[op], 0) + 1
    print(f">> benchmark coverage OK: {len(measured)}/{total} operations measured "
          f"({100 * len(measured) / total:.1f}%), {len(skipped)} skipped with a reason")
    for reason, n in sorted(by_reason.items(), key=lambda kv: (-kv[1], kv[0])):
        print(f"   {n:4}  {reason:16} {SKIP_REASONS[reason]}")


if __name__ == "__main__":
    main()
