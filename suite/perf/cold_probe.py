#!/usr/bin/env python3
"""Cold-request leg (workstream H2): first-request latency on a fresh process.

Cold start is a real user experience — server restart, first browse — and a
legitimate Rust-vs-.NET story (no JIT, no tier-0 code to re-warm), so it is
published as a first-class metric ALONGSIDE warm, explicitly labeled, never
blended into the steady-state numbers.

Per-endpoint cold is only honest if the server is actually cold for that
endpoint: hitting endpoint A warms shared state (DB pool, page cache, JIT'd
shared code) for endpoint B. run.sh therefore RESTARTS the server before each
sentinel's probe and calls this once per endpoint:

    python3 cold_probe.py --target ferrofin --base URL --endpoint items_sortname

Measures the first BENCH_COLD_REQUESTS sequential requests (each timed
individually — the first one IS the metric, the rest show the warm-up curve)
and appends into results/raw/<target>-cold-requests.json. Timing is plain
Python around a blocking request: cold latencies are dominated by real work
(ms to seconds), so sub-ms client overhead is noise-level here — no load
engine needed or wanted for N=10 sequential requests.
"""

import argparse
import json
import sys
import time
from pathlib import Path

import benchlib
from bootstrap import load_ctx
from config import CONFIG
from endpoints import BY_NAME
from vegeta import percentile

RAW = Path(__file__).resolve().parent / "results" / "raw"


def probe(base, e, ctx, count):
    """First-`count` sequential requests → per-request ms (expected-status only
    enters the latency list; errors are counted, never timed as successes)."""
    lat, bad = [], 0
    for _ in range(count):
        t0 = time.perf_counter()
        status, _body = benchlib.fire(base, e, ctx, timeout=120)
        ms = (time.perf_counter() - t0) * 1000
        if status == e["ok"]:
            lat.append(round(ms, 2))
        else:
            bad += 1
    return lat, bad


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--target", required=True, choices=["ferrofin", "jellyfin"])
    ap.add_argument("--base", required=True)
    ap.add_argument("--endpoint", required=True)
    args = ap.parse_args()

    e = BY_NAME.get(args.endpoint)
    if e is None:
        sys.exit(f"unknown endpoint {args.endpoint!r}")
    # Tokens and item ids are DB-backed, so the ctx from the warm leg survives
    # the restart — no fresh login (which would itself warm the auth path).
    ctx = load_ctx(args.target)
    if ctx is None:
        sys.exit(f"no {args.target}-ctx.json — run the warm leg (compare.py) first")

    lat, bad = probe(args.base, e, ctx, CONFIG["BENCH_COLD_REQUESTS"])
    row = {
        "first": lat[0] if lat else None,          # THE cold number
        "p50": round(percentile(sorted(lat), 50), 2) if lat else None,
        "max": max(lat) if lat else None,
        "all": lat,                                 # the warm-up curve, in order
        "bad": bad,
    }

    out_path = RAW / f"{args.target}-cold-requests.json"
    try:
        doc = json.loads(out_path.read_text())
    except OSError:
        doc = {"target": args.target, "requests_per_endpoint": CONFIG["BENCH_COLD_REQUESTS"],
               "endpoints": {}}
    doc["endpoints"][args.endpoint] = row
    out_path.write_text(json.dumps(doc, indent=2) + "\n")
    print(f"[{args.target}] cold {args.endpoint}: first={row['first']} ms "
          f"p50={row['p50']} ms ({len(lat)}/{len(lat) + bad} ok)")


if __name__ == "__main__":
    main()
