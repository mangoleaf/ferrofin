#!/usr/bin/env python3
"""Phase C — the MIXED contention run. Port of the retired ``phase-c.js``.

All endpoints are hit concurrently in a closed VU loop — NOT for per-endpoint
numbers (Phase A's open-loop vegeta legs give those), but to expose the
cross-endpoint interference that isolation hides: the shared DB pool, locks,
caches.

Deliberately CLOSED-LOOP, unlike the vegeta comparison legs: this phase
measures how a fixed client population contends for shared server resources,
and a closed model (N clients, each waiting for its response before issuing
the next request) is exactly that population. The open-model
coordinated-omission argument (vegeta.py's docstring) applies to per-endpoint
latency comparison, not here — so Python threads driving blocking urllib
requests are acceptable; latency precision matters less by design.

The driver only loads a ready context (bootstrap.ready_ctx — the shell
scripts' bringup_scan has already provisioned + scanned), so run-phase-c.sh
can read cgroup memory.peak around the load window.

Usage::

    python3 phase_c.py --target ferrofin --base http://localhost:18196

Knobs: BENCH_VUS (default 50), BENCH_DURATION (default 30s; '30s' or '30').
Writes results/raw/phaseC-<target>.json.
"""

import argparse
import json
import os
import threading
import time
from pathlib import Path

import bootstrap
from benchlib import PooledClient
from endpoints import ENDPOINTS
from vegeta import percentile

RAW = Path(__file__).resolve().parent / "results" / "raw"

# Own-window rows (auth_login) skip the mixed loop — the login storm gets its
# own measured window elsewhere (PBKDF2 saturates CPU and invalidates caches).
MIXED = [e for e in ENDPOINTS if not e["scenario"]]


def parse_duration(raw, default=30):
    """Accept the k6-style '30s' as well as a bare '30' (seconds)."""
    try:
        return int(str(raw).strip().rstrip("s") or default)
    except ValueError:
        return default


def jnum(x):
    """Drop the trailing .0 so the JSON matches what the k6 legs emitted
    (JS numbers have no int/float distinction)."""
    return int(x) if isinstance(x, float) and x.is_integer() else x


def run_closed_loop(base, ctx, vus, duration_secs):
    """The shared mixed closed loop (phase C and the pool sweep).

    Each VU thread loops over all non-scenario endpoints in lockstep TABLE
    ORDER — every endpoint gets the same request pressure, so no row can win
    by being sampled while the others rest. Latency is wall time around
    benchlib.fire (connect + request + read body).

    Returns {name: {"lat": [ms...], "ok": n, "total": n}} merged across VUs.
    Only expected-status responses enter "lat" (an error path is cheap and
    would fake a win); ok/total expose the rest.
    """
    deadline = time.monotonic() + duration_secs
    per_vu = [{e["name"]: {"lat": [], "ok": 0, "total": 0} for e in MIXED}
              for _ in range(vus)]

    def worker(mine):
        # One persistent keep-alive connection per VU — a fresh TCP connect
        # per request makes the CLIENT the contended resource at 50 threads
        # and measures connect overhead, not the server (review, round 1).
        conn = PooledClient(base)
        try:
            while time.monotonic() < deadline:
                for e in MIXED:
                    # Check per request, not per pass: a full pass under contention
                    # can take many seconds, and overshooting a whole pass would
                    # stretch the measured window unevenly across VUs.
                    if time.monotonic() >= deadline:
                        return
                    t0 = time.perf_counter()
                    status, _ = conn.fire(e, ctx)
                    ms = (time.perf_counter() - t0) * 1000
                    row = mine[e["name"]]
                    row["total"] += 1
                    if status == e["ok"]:
                        row["ok"] += 1
                        row["lat"].append(ms)
        finally:
            conn.close()

    threads = [threading.Thread(target=worker, args=(per_vu[i],), daemon=True)
               for i in range(vus)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    merged = {e["name"]: {"lat": [], "ok": 0, "total": 0} for e in MIXED}
    for mine in per_vu:
        for name, row in mine.items():
            m = merged[name]
            m["lat"].extend(row["lat"])
            m["ok"] += row["ok"]
            m["total"] += row["total"]
    return merged


def endpoint_summary(row, with_rps=False, duration_secs=None):
    """One endpoint's summary row (the k6 handleSummary shape)."""
    if row is None or not row["total"]:
        out = {"p50": None, "p95": None, "p99": None, "count": 0, "okPct": 0}
        if with_rps:
            out["rps"] = 0
        return out
    lat = sorted(row["lat"])
    out = {
        "p50": jnum(round(percentile(lat, 50), 2)) if lat else None,
        "p95": jnum(round(percentile(lat, 95), 2)) if lat else None,
        "p99": jnum(round(percentile(lat, 99), 2)) if lat else None,
        "count": len(lat),
        "okPct": jnum(round(100 * row["ok"] / row["total"], 1)),
    }
    if with_rps:
        out["rps"] = jnum(round(len(lat) / duration_secs, 1)) if duration_secs else 0
    return out


def main():
    ap = argparse.ArgumentParser(description="Phase C mixed contention run")
    ap.add_argument("--target", required=True, choices=["ferrofin", "jellyfin"])
    ap.add_argument("--base", required=True)
    args = ap.parse_args()

    vus = int(os.environ.get("BENCH_VUS", "50"))
    duration_secs = parse_duration(os.environ.get("BENCH_DURATION", "30s"))

    ctx = bootstrap.ready_ctx(args.target, args.base)
    print(f"[{args.target}] phase C: {vus} VUs x {duration_secs}s mixed load", flush=True)
    merged = run_closed_loop(args.base, ctx, vus, duration_secs)

    out = {
        "target": args.target,
        "vus": vus,
        "durationSec": duration_secs,
        # All ENDPOINTS (scenario rows get the null shape), matching the JS.
        "endpoints": {e["name"]: endpoint_summary(merged.get(e["name"]))
                      for e in ENDPOINTS},
    }
    RAW.mkdir(parents=True, exist_ok=True)
    (RAW / f"phaseC-{args.target}.json").write_text(json.dumps(out) + "\n")


if __name__ == "__main__":
    main()
