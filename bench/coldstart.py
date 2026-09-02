#!/usr/bin/env python3
"""Cold start = restart of a provisioned server → first authenticated 200 on the
home-screen query (PLAN_BENCHMARK_V3 D1). stdlib only.

    coldstart.py CONTAINER URL IDS_JSON OUT_JSON [REPS=5] [POLL_MS=10]

t0 is the container process start (`docker inspect .State.StartedAt`, ns precision), so
docker's own stop/spawn overhead is excluded. Polls GET /UserViews every POLL_MS with the
token persisted in the test data. Also records (unpublished) the first 200 from
/System/Info/Public.
"""

import datetime as dt
import http.client
import json
import subprocess
import sys
import time
import urllib.error
import urllib.request


def started_at(container):
    s = subprocess.check_output(["docker", "inspect", "-f", "{{.State.StartedAt}}", container], text=True).strip()
    # RFC3339 with up to 9 fractional digits; Python parses 6 — keep the rest by hand.
    head, _, frac = s[:-1].partition(".")  # RFC3339Nano omits the fraction when it is zero
    return dt.datetime.fromisoformat(head).replace(tzinfo=dt.timezone.utc).timestamp() + int((frac or "0").ljust(9, "0")) / 1e9


def poll(url, headers, timeout_s, poll_s):
    t_end = time.time() + timeout_s
    while time.time() < t_end:
        try:
            with urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=1) as r:
                if r.status == 200:
                    return time.time()
        except (urllib.error.HTTPError, urllib.error.URLError, OSError, http.client.HTTPException):
            pass
        time.sleep(poll_s)
    return None


def main():
    container, url, ids_path, out = sys.argv[1], sys.argv[2].rstrip("/"), sys.argv[3], sys.argv[4]
    reps = int(sys.argv[5]) if len(sys.argv) > 5 else 5
    poll_s = (int(sys.argv[6]) if len(sys.argv) > 6 else 10) / 1000
    ids = json.load(open(ids_path))
    hdr = {"Authorization": f'MediaBrowser Client="bench", Device="bench", DeviceId="bench-cold", Version="3", Token="{ids["token"]}"'}
    runs = []
    for i in range(reps):
        r0 = time.time()
        subprocess.run(["docker", "restart", "-t", "60", container], check=True, stdout=subprocess.DEVNULL)
        restart_s = time.time() - r0
        t0 = started_at(container)
        poll_from_ms = (time.time() - t0) * 1000  # how late after process start polling began
        t_pub = poll(url + "/System/Info/Public", {}, 300, poll_s)
        t_home = poll(url + f"/UserViews?userId={ids['user']}", hdr, 300, poll_s)
        rec = {"started_at": t0, "docker_restart_s": restart_s, "poll_from_ms": poll_from_ms,
               "public_ms": None if t_pub is None else (t_pub - t0) * 1000,
               "home_ms": None if t_home is None else (t_home - t0) * 1000}
        runs.append(rec)
        print(f"restart {i}: home {rec['home_ms'] and round(rec['home_ms'])} ms (public {rec['public_ms'] and round(rec['public_ms'])} ms)", flush=True)
        time.sleep(5)
    json.dump({"poll_ms": poll_s * 1000, "runs": runs}, open(out, "w"), indent=1)


if __name__ == "__main__":
    main()
