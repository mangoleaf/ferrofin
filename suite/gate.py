#!/usr/bin/env python3
"""suite/gate.py — the regression gate, reading the merged run record (Plan 4 + Plan 6 step 6).

Compares the latest merged run (suite/results/runs.json → last entry) against
suite/perf-baseline.json and FAILS (exit 1) when, for any benched op:
  - Hermit p50/p95/p99 exceeds baseline × factor (PERF_GATE_FACTOR, default 1.5), or
  - Hermit's 200-rate is below 100%, or
  - an op that was `deep_verified` in the baseline has regressed to unverified
    (parity and perf now gate each other — the reason the merged suite exists).

  python3 suite/gate.py               check latest run vs baseline
  python3 suite/gate.py --rebaseline  write baseline from the latest run
"""
import json
import os
import sys
from pathlib import Path

RESULTS = Path(__file__).resolve().parent / "results"
BASELINE = Path(__file__).resolve().parent / "perf-baseline.json"
FACTOR = float(os.environ.get("PERF_GATE_FACTOR", "1.5"))


def latest_run():
    runs = json.loads((RESULTS / "runs.json").read_text())["runs"]
    live = [r for r in runs if not r["meta"].get("legacy")]
    if not live:
        sys.exit("gate: no non-legacy run in suite/results/runs.json — run `suite/run.sh all` first")
    return live[-1]


def rebaseline(run):
    # Keyed by VARIANT (each /Items variant has its own latency); deep_verified is its op's.
    variants = {}
    for o in run["operations"]:
        p = o["perf"]
        if p["h_p50"] is None:
            continue
        variants[p["variant"]] = {"op": o["op"], "h_p50": p["h_p50"], "h_p95": p["h_p95"],
                                  "h_p99": p["h_p99"], "deep_verified": o["parity"]["deep_verified"]}
    BASELINE.write_text(json.dumps({"factor": FACTOR, "hermit": run["meta"]["hermit"],
                                    "variants": variants}, indent=2) + "\n")
    print(f">> wrote {BASELINE.name}: {len(variants)} variants baselined at {run['meta']['hermit']}")


def check(run):
    if not BASELINE.exists():
        sys.exit("gate: no baseline — run `suite/run.sh gate --rebaseline` once to establish one")
    base = json.loads(BASELINE.read_text())["variants"]
    fails = []
    for o in run["operations"]:
        op, p, par = o["op"], o["perf"], o["parity"]
        b = base.get(p["variant"])
        if b is None:
            continue
        if p["h_ok"] is not None and p["h_ok"] < 100:
            fails.append(f"{p['variant']}: Hermit 200-rate {p['h_ok']}% < 100%")
        for pct in ("h_p50", "h_p95", "h_p99"):
            if p[pct] is not None and b[pct] and p[pct] > b[pct] * FACTOR:
                fails.append(f"{p['variant']} {pct}: {p[pct]} > {b[pct]}×{FACTOR} (={round(b[pct]*FACTOR,1)})")
        if b.get("deep_verified") and not par["deep_verified"]:
            fails.append(f"{op} ({p['variant']}): parity regressed — was deep_verified, now "
                         f"{par['depth']}/unverified")

    if fails:
        print(f"PERF/PARITY GATE FAILED ({len(fails)} regressions vs baseline):", file=sys.stderr)
        for f in fails:
            print(f"  ✗ {f}", file=sys.stderr)
        sys.exit(1)
    print(f">> gate OK: {len(base)} baselined ops within {FACTOR}× and parity held "
          f"(run {run['meta']['hermit']})")


if __name__ == "__main__":
    run = latest_run()
    if "--rebaseline" in sys.argv:
        rebaseline(run)
    else:
        check(run)
