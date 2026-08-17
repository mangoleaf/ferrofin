#!/usr/bin/env python3
"""Phase A leg — isolated, open-model, one endpoint at a time (port of phase-a.js).

Why this shape (see the research write-up in RESEARCH/benchmark-methodology):

* ISOLATED: only this one endpoint is driven, so its p50/95/99 and the server
  CPU it burns are attributable to that handler, not to interference from the
  other endpoints in the table.
* OPEN MODEL: a constant *arrival rate* (vegeta ``-rate``), not a closed VU
  loop. Requests are dispatched on a fixed schedule regardless of how fast the
  server answers, so a stall inflates the tail of every request that *should*
  have gone out — avoiding coordinated omission (Gil Tene) that a
  think-time-free closed loop hides.

The orchestrator (run-phase-a.sh) brings the server up + scans ONCE
(bootstrap.py, whose ctx this leg reuses from results/raw/<target>-ctx.json),
then runs this per endpoint, snapshotting the container's cgroup cpu.stat
before/after so it can attribute CPU-seconds-per-request. This script only
measures latency: a discarded warm-up window at the same rate (JIT fairness
for .NET), then the measured window.

Output JSON (one file, consumed by run-phase-a.sh's cpu_us patch, by
run-phase-b.sh's sustained check, and by render_phases.py):

    target, endpoint, rate, dur      identity of the run
    p50/p95/p99                      ms, over expected-status responses only
    count                            expected-status responses in the window
    reqs                             total dispatched over warmup+measure — the
                                     denominator for CPU-per-request, since the
                                     orchestrator snapshots cgroup cpu.stat
                                     around the whole run
    dropped                          arrivals the schedule owed but never
                                     dispatched: max(0, rate*dur - dispatched)
                                     (k6's dropped_iterations equivalent)
    okPct, achieved_rate             honesty flags — a row with a low ok-rate
                                     or a generator that couldn't hold the
                                     schedule must be read as saturated/broken,
                                     never as a fast server
"""

import argparse
import json
from pathlib import Path

import vegeta
from bootstrap import ready_ctx
from endpoints import BY_NAME


def secs(v):
    """A duration CLI value in seconds; tolerates a trailing 's' ('20s')."""
    return float(str(v).rstrip("s"))


def run(target, base, name, rate, dur, warmup):
    """Warm up then measure one endpoint open-loop → the output record."""
    if name not in BY_NAME:
        raise SystemExit(f"unknown endpoint: {name}")
    ep = BY_NAME[name]
    ctx = ready_ctx(target, base)
    targets = vegeta.build_targets(base, ep, ctx)

    # Warm-up arrivals at the same rate: dispatched, not recorded (except into
    # `reqs`, the CPU-per-request denominator — the cgroup snapshot brackets
    # the warm-up too).
    warm_records = vegeta.attack(targets, rate, warmup) if warmup > 0 else []
    records = vegeta.attack(targets, rate, dur)

    out = vegeta.summarize(records, ep["ok"], dur, rate=rate)
    out.update({
        "target": target, "endpoint": name, "rate": rate, "dur": dur,
        "reqs": len(warm_records) + len(records),
        # Open-loop honesty: the schedule owed rate*dur arrivals; anything the
        # generator could not dispatch is a drop (server saturated the
        # generator's connections), mirroring k6's dropped_iterations.
        "dropped": max(0, round(rate * dur) - len(records)),
    })
    return out


def main():
    """CLI: measure one endpoint and write the JSON record."""
    ap = argparse.ArgumentParser(description="Phase A: one isolated open-loop endpoint window")
    ap.add_argument("--target", required=True, choices=["ferrofin", "jellyfin"])
    ap.add_argument("--base", required=True, help="server base URL")
    ap.add_argument("-e", "--endpoint", required=True, help="ENDPOINTS name")
    ap.add_argument("--rate", type=int, default=50, help="arrivals/sec (open model)")
    ap.add_argument("--dur", type=secs, default=20.0, help="measured window, seconds")
    ap.add_argument("--warmup", type=secs, default=5.0, help="discarded warm-up, seconds")
    ap.add_argument("--out", default=None,
                    help="output JSON path (default results/raw/phaseA-<target>-<endpoint>.json)")
    args = ap.parse_args()

    out = run(args.target, args.base, args.endpoint, args.rate, args.dur, args.warmup)
    path = Path(args.out or f"results/raw/phaseA-{args.target}-{args.endpoint}.json")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(out) + "\n")
    print(f"[{args.target}] {args.endpoint}: p50={out['p50']} p99={out['p99']} "
          f"ok={out['count']}/{out['reqs']} dropped={out['dropped']}")


if __name__ == "__main__":
    main()
