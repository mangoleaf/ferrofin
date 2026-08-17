#!/usr/bin/env python3
"""Driver for the DB pool-size sweep (pool-sweep.sh). Port of ``pool-sweep.js``.

Same mixed closed loop as phase_c.run_closed_loop — and closed ON PURPOSE:
the pool-size question is "how does a fixed client population queue on
connection ACQUISITION", which only a closed model (N VUs in lockstep over
every endpoint) poses. The open-model vegeta legs saturate one hot query and
reward pool≈cores; real dashboards produce this mixed regime instead, so the
FERROFIN_DB_POOL default is decided here (see pool-sweep.sh's header). Python
threads driving pooled keep-alive connections (benchlib.PooledClient, one per
VU via phase_c.run_closed_loop) are acceptable — latency precision matters
less here by design. NOTE: the "auto = cores is optimal" conclusion predates
this client — re-derive on an idle host before relying on it.

Runs against an ALREADY provisioned + scanned Ferrofin (the sweep scans once
and reuses the volume across pool sizes), so setup only authenticates, picks
the deterministic item, and enriches — bootstrap.ready_ctx does exactly that,
reusing results/raw/ferrofin-ctx.json when the token still works.

Usage::

    python3 pool_sweep.py --base http://localhost:18296 --pool 8

Knobs: BENCH_VUS (default 50), BENCH_DURATION (default 30s),
BENCH_WARMUP_SECONDS (default 10). Writes results/raw/pool-<N>-summary.json.
"""

import argparse
import json
import os
import time

import bootstrap
from benchlib import fire
from endpoints import ENDPOINTS
from phase_c import MIXED, RAW, endpoint_summary, parse_duration, run_closed_loop


def warmup(base, ctx, seconds):
    """Single-threaded warm pass over the mixed set (the k6 setup() warm loop):
    fills caches and the pool before the measured window."""
    until = time.monotonic() + seconds
    while time.monotonic() < until:
        for e in MIXED:
            fire(base, e, ctx)


def main():
    ap = argparse.ArgumentParser(description="DB pool-size sweep, one pool size")
    ap.add_argument("--base", required=True)
    ap.add_argument("--pool", required=True, type=int)
    args = ap.parse_args()

    vus = int(os.environ.get("BENCH_VUS", "50"))
    duration_secs = parse_duration(os.environ.get("BENCH_DURATION", "30s"))
    warm_secs = int(os.environ.get("BENCH_WARMUP_SECONDS", "10"))

    ctx = bootstrap.ready_ctx("ferrofin", args.base)
    if not ctx.get("itemId"):
        raise RuntimeError("library is empty — pool-sweep.sh must scan before sweeping")

    warmup(args.base, ctx, warm_secs)
    print(f"pool={args.pool}: {vus} VUs x {duration_secs}s mixed load", flush=True)
    merged = run_closed_loop(args.base, ctx, vus, duration_secs)

    out = {
        "pool": args.pool,
        "durationSec": duration_secs,
        "endpoints": {e["name"]: endpoint_summary(merged.get(e["name"]),
                                                  with_rps=True, duration_secs=duration_secs)
                      for e in ENDPOINTS},
    }
    RAW.mkdir(parents=True, exist_ok=True)
    (RAW / f"pool-{args.pool}-summary.json").write_text(json.dumps(out, indent=2) + "\n")
    print(f"\npool={args.pool} done", flush=True)


if __name__ == "__main__":
    main()
