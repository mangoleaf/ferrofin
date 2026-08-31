#!/usr/bin/env python3
"""Sentinel measurements for the fast regression gate — replaces perf-gate.js.

perf-gate.sh brings Ferrofin up, then calls this once per sentinel endpoint:

    python3 perf_gate.py --base http://localhost:18196 --endpoint items_sortname \
        --rate 25 --secs 10

Open-loop like the comparison leg (workstream G): a fixed arrival rate, not a
VU loop, so a regression shows up as latency instead of being absorbed by the
generator slowing down. Writes results/raw/perfgate-ferrofin-<name>.json in
the shape gate.py compare-raw reads ({p50,p95,p99,ok,bad}).
"""

import argparse
import json
import sys
from pathlib import Path

import vegeta
from bootstrap import ready_ctx
from config import CONFIG
from endpoints import BY_NAME

RAW = Path(__file__).resolve().parent / "results" / "raw"


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--base", required=True)
    ap.add_argument("--endpoint", required=True)
    ap.add_argument("--rate", type=int, required=True)
    ap.add_argument("--secs", type=int, required=True)
    args = ap.parse_args()

    e = BY_NAME.get(args.endpoint)
    if e is None:
        sys.exit(f"unknown endpoint {args.endpoint!r}")
    ctx = ready_ctx("ferrofin", args.base)

    targets = vegeta.build_targets(args.base, e, ctx)
    records = vegeta.attack(targets, args.rate, args.secs)
    s = vegeta.summarize(records, e["ok"], args.secs, rate=args.rate)
    out = {
        "endpoint": args.endpoint,
        "p50": s["p50"], "p95": s["p95"], "p99": s["p99"],
        "ok": s["count"], "bad": len(records) - s["count"],
        "rate": args.rate, "secs": args.secs,
        "rate_held": s["achieved_rate"] >= CONFIG["BENCH_RATE_TOLERANCE"] * args.rate,
    }
    RAW.mkdir(parents=True, exist_ok=True)
    (RAW / f"perfgate-ferrofin-{args.endpoint}.json").write_text(json.dumps(out, indent=2) + "\n")
    if not out["rate_held"]:
        # A gate window that couldn't hold its schedule is not a valid sample.
        sys.exit(f"perf_gate: {args.endpoint} achieved {s['achieved_rate']}/s "
                 f"of target {args.rate}/s — rerun or lower the gate rate")


if __name__ == "__main__":
    main()
