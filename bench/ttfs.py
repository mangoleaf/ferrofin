#!/usr/bin/env python3
"""Time to first playable segment (PLAN_BENCHMARK_V3 D2). stdlib only.

    ttfs.py URL IDS_JSON OUT_JSON [REPS=5]

HLS: POST PlaybackInfo with a device profile that cannot direct-play the file (vp9/webm
only, 2 Mbps cap on an 8 Mbps h264 source) → GET master.m3u8 → GET the variant playlist
→ GET the first segment; the clock stops when the segment body is fully received.
Direct play: GET /Videos/{id}/stream?static=true with Range 0-1MiB; time to first byte.
Each HLS rep uses a fresh PlaySessionId and kills its encoding afterwards.
"""

import http.client
import json
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

DEVICE = "bench-ttfs"
PROFILE = {
    "Name": "bench-ttfs", "MaxStreamingBitrate": 2_000_000, "MaxStaticBitrate": 100_000_000,
    "DirectPlayProfiles": [{"Container": "webm", "Type": "Video", "VideoCodec": "vp9", "AudioCodec": "opus"}],
    "TranscodingProfiles": [{"Container": "ts", "Type": "Video", "AudioCodec": "aac", "VideoCodec": "h264",
                             "Context": "Streaming", "Protocol": "hls", "MaxAudioChannels": "2",
                             "MinSegments": 1, "BreakOnNonKeyFrames": True}],
    "CodecProfiles": [], "ResponseProfiles": [], "SubtitleProfiles": [],
}


def main():
    url, ids_path, out = sys.argv[1].rstrip("/"), sys.argv[2], sys.argv[3]
    reps = int(sys.argv[4]) if len(sys.argv) > 4 else 5
    ids = json.load(open(ids_path))
    hdr = {"Authorization": f'MediaBrowser Client="bench", Device="bench", DeviceId="{DEVICE}", Version="3", Token="{ids["token"]}"'}

    def req(method, path, body=None, extra=None):
        data = json.dumps(body).encode() if body is not None else None
        r = urllib.request.Request(url + path if path.startswith("/") else path, data=data, method=method, headers={**hdr, **(extra or {})})
        if data is not None:
            r.add_header("Content-Type", "application/json")
        return urllib.request.urlopen(r, timeout=120)

    item, src = ids["stream"], ids["stream_source"]
    result = {"hls": [], "direct": []}
    for i in range(reps):
        ps = f"ttfs{int(time.time() * 1000)}{i}"
        rec = {"play_session": ps}
        t0 = time.perf_counter()
        try:
            with req("POST", f"/Items/{item}/PlaybackInfo?" + urllib.parse.urlencode(
                    {"UserId": ids["user"], "StartTimeTicks": 0, "IsPlayback": "true", "AutoOpenLiveStream": "true",
                     "MaxStreamingBitrate": 2_000_000, "MediaSourceId": src, "SubtitleStreamIndex": -1}),
                     {"DeviceProfile": PROFILE, "PlaySessionId": ps}) as r:
                pi = json.load(r)
            rec["playbackinfo_ms"] = (time.perf_counter() - t0) * 1000
            ms = pi["MediaSources"][0]
            turl = ms.get("TranscodingUrl")
            rec["transcoding_url"] = turl
            rec["play_session"] = pi.get("PlaySessionId", ps)
            if not turl:
                rec["error"] = "server did not offer a transcode (SupportsTranscoding=%s, SupportsDirectPlay=%s)" % (ms.get("SupportsTranscoding"), ms.get("SupportsDirectPlay"))
                result["hls"].append(rec)
                continue
            with req("GET", url + turl) as r:
                master = r.read().decode()
            rec["master_ms"] = (time.perf_counter() - t0) * 1000
            variant = next(l for l in master.splitlines() if l and not l.startswith("#"))
            vurl = urllib.parse.urljoin(url + turl, variant)
            with req("GET", vurl) as r:
                playlist = r.read().decode()
            rec["variant_ms"] = (time.perf_counter() - t0) * 1000
            seg = next(l for l in playlist.splitlines() if l and not l.startswith("#"))
            with req("GET", urllib.parse.urljoin(vurl, seg)) as r:
                first = r.read(1)
                rec["segment_first_byte_ms"] = (time.perf_counter() - t0) * 1000
                body = first + r.read()
            rec["ttfs_ms"] = (time.perf_counter() - t0) * 1000
            rec["segment_bytes"] = len(body)
        except (OSError, http.client.HTTPException, StopIteration, KeyError, IndexError, ValueError) as e:
            rec["error"] = f"{type(e).__name__}: {e}"
        finally:
            try:
                req("DELETE", f"/Videos/ActiveEncodings?deviceId={DEVICE}&playSessionId={rec['play_session']}").close()
            except (OSError, http.client.HTTPException):
                pass
        result["hls"].append(rec)
        print(f"hls rep {i}: {rec.get('ttfs_ms', rec.get('error'))}", flush=True)
        time.sleep(3)

    for i in range(reps):
        rec = {}
        t0 = time.perf_counter()
        try:
            with req("GET", f"/Videos/{item}/stream?static=true&mediaSourceId={src}", extra={"Range": "bytes=0-1048575"}) as r:
                r.read(1)
                rec["ttfb_ms"] = (time.perf_counter() - t0) * 1000
                r.read()
                rec["total_ms"] = (time.perf_counter() - t0) * 1000
                rec["status"] = r.status
        except (OSError, http.client.HTTPException, ValueError) as e:
            rec["error"] = f"{type(e).__name__}: {e}"
        result["direct"].append(rec)
        print(f"direct rep {i}: {rec.get('ttfb_ms', rec.get('error'))}", flush=True)
    json.dump(result, open(out, "w"), indent=1)


if __name__ == "__main__":
    main()
