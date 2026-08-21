#!/usr/bin/env python3
"""Load driver for the cast + SyncPlay paths, for CPU/heap profiling.

These paths are not in the benchmark (the perf gate's sentinels are all read
endpoints), so this drives them directly while the server runs under samply or
heaptrack. It also reports per-operation latency as a function of N, which is
what actually catches an N+1: a per-group or per-member database round trip
shows up as latency growing linearly with group/member count.

    FERROFIN_BASE=http://127.0.0.1:18099 FERROFIN_USER=admin FERROFIN_PASS= \
      python3 suite/ws/profile_load.py [--reps 200]
"""

import argparse
import os
import statistics
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from wsclient import WS, http  # noqa: E402

USER = os.environ.get("FERROFIN_USER", "admin")
PASS = os.environ.get("FERROFIN_PASS", "")


def login(device_id):
    ident = dict(client=f"Prof-{device_id}", device=device_id, device_id=device_id, version="1")
    status, body = http("POST", "/Users/AuthenticateByName",
                        body={"Username": USER, "Pw": PASS}, **ident)
    if status != 200:
        raise SystemExit(f"login failed: {status} {body!r}")
    return {"token": body["AccessToken"], "session_id": body["SessionInfo"]["Id"],
            "user_id": body["User"]["Id"], "ident": ident}


def timed(fn, reps):
    """Runs fn reps times, returning (p50_ms, p95_ms)."""
    samples = []
    for _ in range(reps):
        t0 = time.perf_counter()
        fn()
        samples.append((time.perf_counter() - t0) * 1000)
    samples.sort()
    return (
        statistics.median(samples),
        samples[min(len(samples) - 1, int(len(samples) * 0.95))],
    )


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--reps", type=int, default=200)
    args = ap.parse_args()

    ctl = login("prof-ctl")
    tgt = login("prof-tgt")
    ws_c = WS(f"/socket?api_key={ctl['token']}&deviceId=prof-ctl")
    ws_t = WS(f"/socket?api_key={tgt['token']}&deviceId=prof-tgt")
    time.sleep(0.4)

    http("POST", "/Sessions/Capabilities/Full", token=tgt["token"],
         body={"PlayableMediaTypes": ["Video", "Audio"], "SupportedCommands": ["Play"],
               "SupportsMediaControl": True, "SupportsPersistentIdentifier": True},
         **tgt["ident"])

    def first_id(types):
        st, body = http("GET", f"/Items?UserId={ctl['user_id']}&Recursive=true&Limit=1"
                              f"&IncludeItemTypes={types}", token=ctl["token"], **ctl["ident"])
        items = (body or {}).get("Items") or [] if st == 200 else []
        return items[0]["Id"] if items else None

    movie = first_id("Movie")
    series = first_id("Series")
    if not movie:
        raise SystemExit("no library content — run suite/ws/seed_library.sh first")

    print(f"reps={args.reps}  movie={movie}  series={series}\n")
    print(f"{'operation':<52} {'p50 ms':>8} {'p95 ms':>8}")
    print("-" * 70)

    def row(name, fn, reps=None):
        p50, p95 = timed(fn, reps or args.reps)
        print(f"{name:<52} {p50:>8.2f} {p95:>8.2f}")
        return p50

    # ---- cast play translation ------------------------------------------
    row("cast: plain movie (1 item, no expansion)",
        lambda: http("POST", f"/Sessions/{tgt['session_id']}/Playing"
                             f"?playCommand=PlayNow&itemIds={movie}",
                     token=ctl["token"], **ctl["ident"]))
    if series:
        row("cast: series (folder expansion)",
            lambda: http("POST", f"/Sessions/{tgt['session_id']}/Playing"
                                 f"?playCommand=PlayNow&itemIds={series}",
                         token=ctl["token"], **ctl["ident"]))
    row("cast: playstate command (no translation)",
        lambda: http("POST", f"/Sessions/{tgt['session_id']}/Playing/Pause",
                     token=ctl["token"], **ctl["ident"]))

    # ---- SyncPlay list, as a function of group count ---------------------
    # Each extra group costs a library-access check; if that check re-fetches
    # the user per group, latency grows linearly instead of staying flat.
    # A session may be in only ONE group, and a group with no members is
    # dropped — so holding N groups open needs N distinct sessions, each
    # parked in its own group with a non-empty queue (an empty queue skips the
    # access check entirely and would measure nothing).
    print()
    holders = []
    for target_groups in (1, 5, 20, 50):
        while len(holders) < target_groups:
            n = len(holders)
            holder = login(f"prof-hold-{n}")
            st, g = http("POST", "/SyncPlay/New", token=holder["token"],
                         body={"GroupName": f"prof-{n}"}, **holder["ident"])
            if st != 200:
                raise SystemExit(f"could not create group: {st} {g!r}")
            http("POST", "/SyncPlay/SetNewQueue", token=holder["token"],
                 body={"PlayingQueue": [movie], "PlayingItemPosition": 0,
                       "StartPositionTicks": 0}, **holder["ident"])
            holders.append(holder)
        st, listed = http("GET", "/SyncPlay/List", token=ctl["token"], **ctl["ident"])
        seen = len(listed or []) if st == 200 else -1
        row(f"syncplay: GET /SyncPlay/List with {target_groups:>3} group(s) [saw {seen:>3}]",
            lambda: http("GET", "/SyncPlay/List", token=ctl["token"], **ctl["ident"]),
            reps=max(20, args.reps // 4))

    # ---- SyncPlay queue change, with the caller in a group ----------------
    print()
    row("syncplay: SetNewQueue (per-member access check)",
        lambda: http("POST", "/SyncPlay/SetNewQueue", token=ctl["token"],
                     body={"PlayingQueue": [movie], "PlayingItemPosition": 0,
                           "StartPositionTicks": 0}, **ctl["ident"]),
        reps=max(20, args.reps // 4))
    row("syncplay: Pause (no access check)",
        lambda: http("POST", "/SyncPlay/Pause", token=ctl["token"], **ctl["ident"]),
        reps=max(20, args.reps // 4))

    http("POST", "/SyncPlay/Leave", token=ctl["token"], **ctl["ident"])
    ws_c.close()
    ws_t.close()
    print("\ndone")


if __name__ == "__main__":
    main()
