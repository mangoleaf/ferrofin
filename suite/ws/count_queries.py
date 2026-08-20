#!/usr/bin/env python3
"""Counts the SQL queries one request costs, for the cast + SyncPlay paths.

These paths are database-bound, so "queries per request" localizes a regression
far better than a CPU flamegraph: an N+1 shows up as a count that scales with
group or member count instead of staying flat.

Run the server with `RUST_LOG='info,sqlx::query=debug'` writing to a log file,
then:

    FERROFIN_BASE=... FERROFIN_LOG=<logfile> python3 suite/ws/count_queries.py
"""

import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from wsclient import http  # noqa: E402

USER = os.environ.get("FERROFIN_USER", "admin")
PASS = os.environ.get("FERROFIN_PASS", "")
LOG = os.environ["FERROFIN_LOG"]


def login(device_id):
    ident = dict(client=f"QC-{device_id}", device=device_id, device_id=device_id, version="1")
    st, body = http("POST", "/Users/AuthenticateByName",
                    body={"Username": USER, "Pw": PASS}, **ident)
    if st != 200:
        raise SystemExit(f"login failed: {st} {body!r}")
    return {"token": body["AccessToken"], "session_id": body["SessionInfo"]["Id"],
            "user_id": body["User"]["Id"], "ident": ident}


def log_size():
    return os.path.getsize(LOG)


def queries_for(fn, settle=0.35):
    """SQL statements executed while fn() runs."""
    time.sleep(settle)
    start = log_size()
    fn()
    time.sleep(settle)
    with open(LOG, "rb") as fh:
        fh.seek(start)
        tail = fh.read().decode("utf-8", "replace")
    return sum(1 for line in tail.splitlines() if '"sqlx::query"' in line or "sqlx::query" in line)


def main():
    ctl = login("qc-ctl")
    tgt = login("qc-tgt")
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

    print(f"{'operation':<52} {'queries':>8}")
    print("-" * 62)

    n = queries_for(lambda: http("POST", f"/Sessions/{tgt['session_id']}/Playing"
                                         f"?playCommand=PlayNow&itemIds={movie}",
                                 token=ctl["token"], **ctl["ident"]))
    print(f"{'cast: plain movie':<52} {n:>8}")

    holders = []
    for target in (1, 5, 20):
        while len(holders) < target:
            h = login(f"qc-hold-{len(holders)}")
            http("POST", "/SyncPlay/New", token=h["token"],
                 body={"GroupName": f"qc-{len(holders)}"}, **h["ident"])
            http("POST", "/SyncPlay/SetNewQueue", token=h["token"],
                 body={"PlayingQueue": [movie], "PlayingItemPosition": 0,
                       "StartPositionTicks": 0}, **h["ident"])
            holders.append(h)
        n = queries_for(lambda: http("GET", "/SyncPlay/List",
                                     token=ctl["token"], **ctl["ident"]))
        print(f"{f'syncplay: List with {target:>3} group(s)':<52} {n:>8}")

    for h in holders:
        http("POST", "/SyncPlay/Leave", token=h["token"], **h["ident"])


if __name__ == "__main__":
    main()
