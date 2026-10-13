#!/usr/bin/env python3
"""Restart-to-authenticated-views with retained host caches; stop time is excluded.

    coldstart.py CONTAINER URL IDS_JSON OUT_JSON [REPS=5] [POLL_MS=10]

The poller starts after stop completes and before docker start. Its first successful
response timestamp is independent of when docker start returns. stdlib only.
"""
import datetime as dt
import http.client
import json
import os
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request


def started_at(container):
    s = subprocess.check_output(["docker", "inspect", "-f", "{{.State.StartedAt}}", container],
                                text=True, timeout=float(os.environ.get("HTTP_TIMEOUT_S", "10"))).strip()
    head, _, frac = s[:-1].partition(".")
    return dt.datetime.fromisoformat(head).replace(tzinfo=dt.timezone.utc).timestamp() + int((frac or "0").ljust(9, "0")) / 1e9


def poll(url, headers, timeout_s, poll_s, stop, started):
    deadline = time.monotonic() + timeout_s
    started.set()
    while not stop.is_set() and time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=min(1, timeout_s)) as r:
                if r.status == 200:
                    return time.time()
        except (urllib.error.URLError, OSError, http.client.HTTPException):
            pass
        stop.wait(poll_s)
    return None


def restart(container, url, headers, poll_s):
    ready = float(os.environ.get("READY_TIMEOUT_S", "300"))
    cleanup = float(os.environ.get("CLEANUP_TIMEOUT_S", "30"))
    subprocess.run(["docker", "stop", "-t", str(int(cleanup)), container], check=True,
                   stdout=subprocess.DEVNULL, timeout=cleanup + 2)
    stop, started = threading.Event(), threading.Event()
    result = []
    thread = threading.Thread(target=lambda: result.append(poll(url, headers, ready, poll_s, stop, started)))
    thread.start()
    try:
        if not started.wait(2):
            raise RuntimeError("readiness poller did not start")
        poll_from = time.time()
        subprocess.run(["docker", "start", container], check=True, stdout=subprocess.DEVNULL, timeout=ready)
        t0 = started_at(container)
        thread.join(ready + 1)
        t_home = result[0] if result else None
        if t_home is None:
            raise RuntimeError("authenticated views readiness timed out")
        if t_home < t0:
            raise RuntimeError("readiness timestamp precedes container start")
        return {"started_at": t0, "poll_from_ms": (poll_from - t0) * 1000,
                "home_ms": (t_home - t0) * 1000}
    finally:
        stop.set()
        thread.join(2)  # each HTTP call is bounded by one second
        if thread.is_alive():
            raise RuntimeError("readiness poller did not stop")


def main():
    container, url, ids_path, out = sys.argv[1], sys.argv[2].rstrip("/"), sys.argv[3], sys.argv[4]
    reps = int(sys.argv[5]) if len(sys.argv) > 5 else 5
    poll_s = (int(sys.argv[6]) if len(sys.argv) > 6 else 10) / 1000
    with open(ids_path) as f:
        ids = json.load(f)
    hdr = {"Authorization": f'MediaBrowser Client="bench", Device="bench", DeviceId="bench-cold", Version="3", Token="{ids["token"]}"'}
    runs = []
    for i in range(reps):
        try:
            rec = restart(container, url + f"/UserViews?userId={ids['user']}", hdr, poll_s)
        except (OSError, subprocess.SubprocessError, RuntimeError) as e:
            rec = {"error": f"{type(e).__name__}: {e}"}
        runs.append(rec)
        with open(out + ".tmp", "w") as f:
            json.dump({"validation": 1, "poll_ms": poll_s * 1000, "runs": runs}, f, indent=1)
        os.replace(out + ".tmp", out)
        print(f"restart {i}: {rec}", flush=True)
        if rec.get("error"):
            break
        if i + 1 < reps:
            time.sleep(5)
    return int(len(runs) != reps or any(r.get("error") for r in runs))


if __name__ == "__main__":
    sys.exit(main())
