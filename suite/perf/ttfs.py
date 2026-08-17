#!/usr/bin/env python3
"""Transcode time-to-first-segment (TTFS), measured as a real client experiences it:
POST PlaybackInfo (real Chrome DeviceProfile, DirectPlay/DirectStream disabled)
-> TranscodingUrl -> master.m3u8 -> variant -> init (if fMP4) + first media segment.

Port of the retired ``transcode.js``. This is a stateful single-client HLS
journey, not an open-loop load shape — so no load engine, just sequential
stdlib requests through benchlib.

Two modes per iteration, each its own metric:
  copy   — profile as-is: the Chrome HLS profile accepts hevc, so both servers
           negotiate a video stream-copy remux. This is realistic "time to play start".
  encode — TranscodingProfile pinned to h264 + AllowVideoStreamCopy=false: a genuine
           4K HEVC -> H.264 software encode, the heavy pipeline path.

Fairness:
- fresh DeviceId per measurement: both servers key transcode sessions/caches on
  device, so a reused id can serve cached segments and fake a near-zero TTFS.
- DELETE /Videos/ActiveEncodings after each measurement so a lingering ffmpeg
  doesn't steal CPU from the next one.
- target = movie with the longest runtime (a real 4K film, never a synthetic
  clip); runtime comes from probing the identical file, so both servers pick
  the same title.
"""

import argparse
import copy
import json
import os
import re
import statistics
import time
from pathlib import Path

import benchlib
import bootstrap

HERE = Path(__file__).resolve().parent

# Encode-mode bitrate cap — jellyfin-web's "1080p" quality rung. Being far below
# the 4K source bitrate is what forces a genuine re-encode on both servers
# (stream copy would exceed the cap): Ferrofin ignores the Allow*StreamCopy
# flags today (parity gap), but the bitrate condition is honored by both.
ENCODE_BITRATE = int(os.environ.get("TTFS_BITRATE", "8000000"))

# The repo's real Chrome device profile fixture — single source of truth.
PROFILE = json.loads(
    (HERE / "../../crates/ferrofin-model/tests/data/DeviceProfile-Chrome.json").read_text())
# Encode mode: same profile but video transcode pinned to h264, so stream copy is impossible.
PROFILE_H264 = copy.deepcopy(PROFILE)
for _t in PROFILE_H264.get("TranscodingProfiles") or []:
    if _t.get("Type") == "Video":
        _t["VideoCodec"] = "h264"

LONG = 240  # seconds — the first 4K encoded segment on 4 capped CPUs takes a while


def client_id(dev):
    """The per-device MediaBrowser identity clause."""
    return f'Client="bench", Device="{dev}", DeviceId="{dev}", Version="1.0"'


def mb(token, dev):
    """Authenticated request headers bound to a specific DeviceId."""
    return {
        "Authorization": f'MediaBrowser Token="{token}", {client_id(dev)}',
        "Content-Type": "application/json",
    }


def auth(base, target, ctx, dev):
    """{'token','userId'} for `dev` — reusing the bootstrap ctx when present.

    Reuse a token minted before the perf load if the compare leg wrote one:
    the auth_login scenario throttles Jellyfin's login, so a fresh auth here
    would fail (500/429)."""
    if ctx:
        return {"token": ctx["token"], "userId": ctx["userId"]}
    status, body = benchlib.request(
        "POST", f"{base}/Users/AuthenticateByName",
        {"Username": benchlib.USER, "Pw": benchlib.PASS},
        {"Content-Type": "application/json", "Authorization": f"MediaBrowser {client_id(dev)}"})
    if status != 200:
        raise RuntimeError(f"[{target}] ttfs auth failed: {status}")
    b = json.loads(body)
    return {"token": b["AccessToken"], "userId": b["User"]["Id"]}


def resolve(base, playlist_url, ref):
    """Resolve a playlist reference (absolute URL, absolute path, or relative)
    against the playlist URL."""
    if ref.startswith("http"):
        return ref
    if ref.startswith("/"):
        return f"{base}{ref}"
    return re.sub(r"[^/]*$", "", playlist_url.split("?")[0]) + ref


def setup(base, target, ctx):
    """Pick the measurement target: the movie with the longest RunTimeTicks
    (ties broken by Name ascending) → {'userId', 'itemId'}."""
    a = auth(base, target, ctx, "bench-ttfs-setup")
    h = mb(a["token"], "bench-ttfs-setup")
    status, body = benchlib.request(
        "GET", f"{base}/Items?userId={a['userId']}&Recursive=true&IncludeItemTypes=Movie&Limit=600",
        headers=h)
    if status != 200:
        raise RuntimeError(f"[{target}] ttfs item query failed: {status} {body[:200]!r}")
    items = json.loads(body).get("Items") or []
    best = None
    for it in items:
        ticks = it.get("RunTimeTicks") or 0
        if (best is None or ticks > (best.get("RunTimeTicks") or 0)
                or (ticks == (best.get("RunTimeTicks") or 0) and it["Name"] < best["Name"])):
            best = it
    if not best or not (best.get("RunTimeTicks") or 0) > 0:
        raise RuntimeError(f"[{target}] ttfs: no movie with a runtime found ({len(items)} items)")
    print(f"[{target}] ttfs target: \"{best['Name']}\""
          f" ({round(best['RunTimeTicks'] / 600_000_000)} min)", flush=True)
    return {"userId": a["userId"], "itemId": best["Id"]}


def _base36(n):
    """Positive int → base-36 string (the JS Date.now().toString(36) run id)."""
    digits = "0123456789abcdefghijklmnopqrstuvwxyz"
    out = ""
    while n:
        n, r = divmod(n, 36)
        out = digits[r] + out
    return out or "0"


# Unique per run: both servers key transcode caches on DeviceId, and a device
# name reused from a *previous* run serves that run's cached segments
# (observed: 40 ms "transcodes").
RUN_ID = _base36(int(time.time() * 1000))


def measure(base, target, boot_ctx, ctx, mode, iteration):
    """One full client play-start against a fresh device; returns elapsed ms
    or None (reason logged)."""
    dev = f"bench-ttfs-{RUN_ID}-{mode}-{iteration}"
    a = auth(base, target, boot_ctx, dev)
    h = mb(a["token"], dev)

    def fail(msg):
        print(f"[{target}] {mode}#{iteration} {msg}", flush=True)
        return None

    body = {
        "DeviceProfile": PROFILE_H264 if mode == "encode" else PROFILE,
        "MediaSourceId": ctx["itemId"],
        "EnableDirectPlay": False,
        "EnableDirectStream": False,
        "EnableTranscoding": True,
        "AllowVideoStreamCopy": mode != "encode",
        "AllowAudioStreamCopy": mode != "encode",
        "AutoOpenLiveStream": True,
    }
    if mode == "encode":
        body["MaxStreamingBitrate"] = ENCODE_BITRATE

    t0 = time.monotonic()
    status, pi_body = benchlib.request(
        "POST", f"{base}/Items/{ctx['itemId']}/PlaybackInfo?userId={ctx['userId']}",
        body, h, timeout=60)
    if status != 200:
        return fail(f"PlaybackInfo {status}: {pi_body[:300]!r}")
    pi = json.loads(pi_body)
    src = (pi.get("MediaSources") or [{}])[0]
    if not src.get("TranscodingUrl"):
        return fail(f"no TranscodingUrl (SupportsTranscoding={src.get('SupportsTranscoding')})")

    master_url = resolve(base, base, src["TranscodingUrl"])
    status, master = benchlib.request("GET", master_url, headers=h, timeout=LONG)
    if status != 200:
        return fail(f"master.m3u8 {status}")
    m = re.search(r"^[^#\s].*$", master.decode(errors="replace"), re.M)
    if not m:
        return fail("master.m3u8 had no variant")

    variant_url = resolve(base, master_url, m.group(0))
    status, variant = benchlib.request("GET", variant_url, headers=h, timeout=LONG)
    if status != 200:
        return fail(f"variant {status}")
    text = variant.decode(errors="replace")

    # fMP4 HLS needs the init segment before the first media segment — part of TTFS.
    init_ref = re.search(r'#EXT-X-MAP:URI="([^"]+)"', text)
    if init_ref:
        status, _ = benchlib.request(
            "GET", resolve(base, variant_url, init_ref.group(1)), headers=h, timeout=LONG)
        if status != 200:
            return fail(f"init segment {status}")
    seg_ref = re.search(r"^[^#\s].*$", text, re.M)
    if not seg_ref:
        return fail("variant had no segment line")
    status, seg = benchlib.request(
        "GET", resolve(base, variant_url, seg_ref.group(0)), headers=h, timeout=LONG)
    if status != 200:
        return fail(f"first segment {status}")
    ms = round((time.monotonic() - t0) * 1000)
    print(f"[{target}] ttfs {mode}#{iteration}: {ms} ms"
          f" (segment {len(seg) / 1e6:.1f} MB)", flush=True)

    # Kill this measurement's ffmpeg so it can't contend with the next one.
    # Twice: once by playSessionId (the jellyfin-web way), once device-scoped
    # with an empty psid — Ferrofin's jobs don't carry a PlaySessionId yet, so
    # only the device-scoped form matches there.
    psid = pi.get("PlaySessionId")
    if psid:
        benchlib.request(
            "DELETE", f"{base}/Videos/ActiveEncodings?deviceId={dev}&playSessionId={psid}",
            headers=h)
    benchlib.request(
        "DELETE", f"{base}/Videos/ActiveEncodings?deviceId={dev}&playSessionId=", headers=h)
    time.sleep(3)
    return ms


def stat(vals):
    """The summary row for one mode: median/min/max over successful runs, else None."""
    if not vals:
        return None
    return {"med": round(statistics.median(vals)), "min": round(min(vals)),
            "max": round(max(vals)), "runs": len(vals)}


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--target", required=True, choices=["ferrofin", "jellyfin"])
    ap.add_argument("--base", required=True)
    ap.add_argument("--iterations", type=int,
                    default=int(os.environ.get("TTFS_ITERATIONS", "3")))
    args = ap.parse_args()

    boot_ctx = bootstrap.load_ctx(args.target)
    ctx = setup(args.base, args.target, boot_ctx)

    samples = {"copy": [], "encode": []}
    for iteration in range(args.iterations):
        for mode in ("copy", "encode"):
            ms = measure(args.base, args.target, boot_ctx, ctx, mode, iteration)
            if ms is not None:
                samples[mode].append(ms)

    out = {"target": args.target, "copy": stat(samples["copy"]),
           "encode": stat(samples["encode"]), "iterations": args.iterations}
    raw = HERE / "results" / "raw"
    raw.mkdir(parents=True, exist_ok=True)
    (raw / f"{args.target}-transcode.json").write_text(json.dumps(out, indent=2) + "\n")

    def show(s):
        return f"{s['med']} ms ({s['runs']}/{args.iterations})" if s else "N/A"

    print(f"{args.target} TTFS copy: {show(out['copy'])} · encode: {show(out['encode'])}")


if __name__ == "__main__":
    main()
