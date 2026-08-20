#!/usr/bin/env python3
"""Does hammering the cast + SyncPlay paths leak? Reports RSS per round.

heaptrack cannot instrument the server here (LD_PRELOAD mode hangs it during
library-watcher startup; runtime attach is unstable and traced the wrapper), so
this answers the question that actually matters — does resident memory plateau
or climb — straight from /proc.

    FERROFIN_BASE=... FERROFIN_PID=<pid> python3 suite/ws/rss_plateau.py [--rounds 6]
"""

import argparse
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from wsclient import WS, http  # noqa: E402

USER = os.environ.get("FERROFIN_USER", "admin")
PASS = os.environ.get("FERROFIN_PASS", "")
PID = os.environ["FERROFIN_PID"]


def rss_kb():
    with open(f"/proc/{PID}/status") as fh:
        for line in fh:
            if line.startswith("VmRSS:"):
                return int(line.split()[1])
    raise SystemExit("process gone")


def login(device_id):
    ident = dict(client=f"RSS-{device_id}", device=device_id, device_id=device_id, version="1")
    st, body = http("POST", "/Users/AuthenticateByName",
                    body={"Username": USER, "Pw": PASS}, **ident)
    if st != 200:
        raise SystemExit(f"login failed: {st} {body!r}")
    return {"token": body["AccessToken"], "session_id": body["SessionInfo"]["Id"],
            "user_id": body["User"]["Id"], "ident": ident}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rounds", type=int, default=6)
    ap.add_argument("--ops", type=int, default=400)
    args = ap.parse_args()

    ctl = login("rss-ctl")
    tgt = login("rss-tgt")
    ws_c = WS(f"/socket?api_key={ctl['token']}&deviceId=rss-ctl")
    ws_t = WS(f"/socket?api_key={tgt['token']}&deviceId=rss-tgt")
    time.sleep(0.4)
    http("POST", "/Sessions/Capabilities/Full", token=tgt["token"],
         body={"PlayableMediaTypes": ["Video"], "SupportedCommands": ["Play"],
               "SupportsMediaControl": True, "SupportsPersistentIdentifier": True},
         **tgt["ident"])

    st, body = http("GET", f"/Items?UserId={ctl['user_id']}&Recursive=true&Limit=1"
                          f"&IncludeItemTypes=Movie", token=ctl["token"], **ctl["ident"])
    items = (body or {}).get("Items") or []
    if not items:
        raise SystemExit("no library content — run suite/ws/seed_library.sh first")
    movie = items[0]["Id"]

    # A group to make the SyncPlay verbs do real work, held by this session.
    http("POST", "/SyncPlay/New", token=ctl["token"],
         body={"GroupName": "rss"}, **ctl["ident"])

    print(f"{'round':>6} {'RSS kB':>10} {'delta kB':>10}")
    print("-" * 30)
    prev = rss_kb()
    print(f"{'start':>6} {prev:>10} {'-':>10}")
    for r in range(1, args.rounds + 1):
        for _ in range(args.ops):
            http("POST", f"/Sessions/{tgt['session_id']}/Playing"
                         f"?playCommand=PlayNow&itemIds={movie}",
                 token=ctl["token"], **ctl["ident"])
            http("POST", "/SyncPlay/SetNewQueue", token=ctl["token"],
                 body={"PlayingQueue": [movie], "PlayingItemPosition": 0,
                       "StartPositionTicks": 0}, **ctl["ident"])
            http("GET", "/SyncPlay/List", token=ctl["token"], **ctl["ident"])
        ws_c.drain()
        ws_t.drain()
        time.sleep(1.0)
        now = rss_kb()
        print(f"{r:>6} {now:>10} {now - prev:>+10}")
        prev = now

    http("POST", "/SyncPlay/Leave", token=ctl["token"], **ctl["ident"])
    ws_c.close()
    ws_t.close()


if __name__ == "__main__":
    main()
