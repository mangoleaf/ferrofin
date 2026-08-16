#!/usr/bin/env python3
"""Phase D — the "what users feel" load. Port of the retired ``phase-d.js``.

A handful of virtual clients behaving like real ones, instead of 50
zero-think-time VUs hammering in lockstep.

Deliberately CLOSED-LOOP, unlike the vegeta comparison legs: a home media
server has a fixed, small user population — a few apps on a few devices — so
the closed model (N clients with think time, each waiting for its screen to
load before the next tap) IS the real workload here, not an approximation of
one. Coordinated omission is the price of that realism and is accepted by
design; Python threads driving blocking urllib requests are fine, latency
precision matters less than the shape of the journey.

Each VU is one client app on its own device (own login + DeviceId — reusing
one DeviceId across VUs makes the servers fold every reporter into a single
session). A session iteration: home screen → browse a library page → open an
item's detail → fetch posters → start playback (PlaybackInfo + playstate
start/progress/stop, exercising the write path) — with 1–3 s think time
between steps.

Run via run-phase-d.sh (which boots + scans the server first). Direct::

    python3 phase_d.py --target ferrofin --base http://localhost:18196

Knobs: PHASE_D_VUS (default 8), PHASE_D_DUR (default 120s; '120s' or '120').
Writes results/raw/phaseD-<target>.json.
"""

import argparse
import json
import os
import random
import threading
import time

import benchlib
from phase_c import RAW, jnum, parse_duration
from vegeta import percentile

# One latency trend per journey step; every request in a step feeds it.
STEPS = ["home", "library", "detail", "images", "playback"]


def login(base, target, vu):
    """Per-VU client identity: own DeviceId, own token. benchlib.authenticate
    uses ONE shared DeviceId, which would fold every VU into a single
    server-side session — so this adapts its request with a per-VU tuple."""
    client_id = (f'Client="bench-d", Device="phone-{vu}", '
                 f'DeviceId="phase-d-vu{vu}", Version="1.0"')
    status, body = benchlib.request(
        "POST", f"{base}/Users/AuthenticateByName",
        {"Username": benchlib.USER, "Pw": benchlib.PASS},
        {"Content-Type": "application/json", "Authorization": f"MediaBrowser {client_id}"})
    if status != 200:
        raise RuntimeError(f"[{target}] vu{vu} auth failed: {status} {body[:200]!r}")
    b = json.loads(body)
    return {
        "userId": b["User"]["Id"],
        "headers": {
            "Authorization": f'MediaBrowser Token="{b["AccessToken"]}", {client_id}',
            "Content-Type": "application/json",
        },
    }


def setup(base, target):
    """Shared item pool for all VUs (the k6 setup())."""
    ctx = benchlib.authenticate(base, target)
    j = benchlib.get_json(
        f"{base}/Items?userId={ctx['userId']}&Recursive=true&IncludeItemTypes=Movie"
        f"&SortBy=SortName&Limit=200", benchlib.token_headers(ctx["token"])) or {}
    items = j.get("Items") or []
    if not items:
        raise RuntimeError("library is empty — run bootstrap.py first")
    return {
        "itemIds": [i["Id"] for i in items],
        "imageIds": [i["Id"] for i in items
                     if (i.get("ImageTags") or {}).get("Primary")][:24],
    }


def vu_worker(base, target, vu, data, deadline, out):
    """One client app: login once, then run whole sessions until the window
    closes. The deadline is checked at session boundaries only — a started
    session runs to completion (k6's gracefulStop analog), so per-step trends
    never contain half-journeys."""
    # Seeded per VU: each client repeats its own item/think sequence run-to-run
    # (reproducibility-ish; cross-VU interleaving still varies).
    rng = random.Random(vu)
    try:
        me = login(base, target, vu)
    except RuntimeError as err:
        out["error"] = str(err)
        return
    uid = me["userId"]
    headers = me["headers"]
    # One persistent keep-alive connection per client app — real clients pool;
    # a per-request TCP connect would bill connect overhead to every step
    # (review, round 1). http.client is not thread-safe: one per VU thread.
    conn = benchlib.PooledClient(base)

    def step(name, requests):
        for method, url, body in requests:
            t0 = time.perf_counter()
            status, _ = conn.request(method, url, body, headers)
            ms = (time.perf_counter() - t0) * 1000
            out["total"] += 1
            # Non-4xx/5xx counts as ok; status 0 (transport error) does NOT —
            # a refused connection returns instantly and would fake a win.
            if 0 < status < 400:
                out["ok"] += 1
                out["steps"][name].append(ms)

    def think():
        time.sleep(1 + rng.random() * 2)

    while time.monotonic() < deadline:
        start = time.perf_counter()
        item_id = data["itemIds"][rng.randrange(len(data["itemIds"]))]

        step("home", [
            ("GET", f"{base}/UserViews?userId={uid}", None),
            ("GET", f"{base}/Items/Latest?userId={uid}&limit=20", None),
            ("GET", f"{base}/UserItems/Resume?userId={uid}&limit=12", None),
            ("GET", f"{base}/Shows/NextUp?userId={uid}&limit=24", None),
        ])
        think()

        page = rng.randrange(4) * 50
        step("library", [
            ("GET", f"{base}/Items?userId={uid}&recursive=true&includeItemTypes=Movie"
                    f"&limit=50&startIndex={page}&sortBy=SortName"
                    f"&fields=PrimaryImageAspectRatio,MediaSourceCount", None),
            ("GET", f"{base}/Genres?userId={uid}", None),
        ])
        think()

        step("detail", [
            ("GET", f"{base}/Items/{item_id}?userId={uid}", None),
            ("GET", f"{base}/Items/{item_id}/Similar?userId={uid}&limit=12", None),
        ])
        step("images", [
            ("GET", f"{base}/Items/{iid}/Images/Primary?maxWidth=400&quality=90", None)
            for iid in data["imageIds"][:3]
        ])
        think()

        # Playback: resolve sources, then the playstate write path a real
        # client drives (start → progress → stop). PositionTicks in 100 ns ticks.
        step("playback", [
            ("GET", f"{base}/Items/{item_id}/PlaybackInfo?userId={uid}", None),
            ("POST", f"{base}/Sessions/Playing",
             {"ItemId": item_id, "PositionTicks": 0, "CanSeek": True,
              "PlayMethod": "DirectPlay"}),
        ])
        time.sleep(1 + rng.random())
        step("playback", [
            ("POST", f"{base}/Sessions/Playing/Progress",
             {"ItemId": item_id, "PositionTicks": 600_000_000, "PlayMethod": "DirectPlay"}),
            ("POST", f"{base}/Sessions/Playing/Stopped",
             {"ItemId": item_id, "PositionTicks": 1_200_000_000}),
        ])

        # Whole-session duration includes the think time — it's the user's
        # wall-clock journey, matching the JS.
        out["sessions"].append((time.perf_counter() - start) * 1000)

    conn.close()


def trend(values, decimals=2, p99=True):
    """p50/p95[/p99]/count summary of a step or session trend, or None."""
    if not values:
        return None
    s = sorted(values)
    out = {"p50": jnum(round(percentile(s, 50), decimals)),
           "p95": jnum(round(percentile(s, 95), decimals))}
    if p99:
        out["p99"] = jnum(round(percentile(s, 99), decimals))
    out["count"] = len(s)
    return out


def main():
    ap = argparse.ArgumentParser(description="Phase D realistic think-time load")
    ap.add_argument("--target", required=True, choices=["ferrofin", "jellyfin"])
    ap.add_argument("--base", required=True)
    args = ap.parse_args()

    vus = int(os.environ.get("PHASE_D_VUS", "8"))
    duration_secs = parse_duration(os.environ.get("PHASE_D_DUR", "120s"), default=120)

    data = setup(args.base, args.target)
    print(f"[{args.target}] phase D: {vus} clients x {duration_secs}s, think time", flush=True)

    deadline = time.monotonic() + duration_secs
    per_vu = [{"steps": {s: [] for s in STEPS}, "sessions": [],
               "ok": 0, "total": 0, "error": None} for _ in range(vus)]
    threads = [threading.Thread(target=vu_worker,
                                args=(args.base, args.target, vu + 1, data, deadline, per_vu[vu]),
                                daemon=True)
               for vu in range(vus)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    errors = [o["error"] for o in per_vu if o["error"]]
    for err in errors:
        print(f"!! {err}", flush=True)
    if len(errors) == vus:
        raise RuntimeError(f"[{args.target}] every VU failed to log in")

    steps = {s: [] for s in STEPS}
    sessions, ok, total = [], 0, 0
    for o in per_vu:
        for s in STEPS:
            steps[s].extend(o["steps"][s])
        sessions.extend(o["sessions"])
        ok += o["ok"]
        total += o["total"]

    out = {
        "target": args.target,
        "steps": {s: trend(steps[s]) for s in STEPS},
        "sessions": trend(sessions, decimals=0, p99=False),
        "okPct": jnum(round(100 * ok / total, 1)) if total else None,
        "vus": vus,
        "durationSec": duration_secs,
    }
    RAW.mkdir(parents=True, exist_ok=True)
    (RAW / f"phaseD-{args.target}.json").write_text(json.dumps(out, indent=2) + "\n")
    print(f"\n[{args.target}] phase D: {json.dumps(out['steps'])}", flush=True)


if __name__ == "__main__":
    main()
