#!/usr/bin/env python3
"""The main comparison leg — replaces the k6 scenario.js.

One invocation drives one server (run.sh calls it once per target):

    python3 compare.py --target ferrofin --base http://localhost:18096
    python3 compare.py --target jellyfin --base http://localhost:18097 --calibrate-rates

Shape of a run (workstreams G + I):

- setup: wizard (jellyfin) → auth → provision → wait for the scan to settle →
  deterministic item pick → context enrichment. The ready context is written to
  results/raw/<target>-ctx.json so later steps (TTFS, fingerprints, item count)
  reuse the pre-storm token instead of grepping logs for it.
- per endpoint, an OPEN-LOOP window: warmup at the measured arrival rate, then
  a measured window at that rate. Rates come from rates.json (calibrated per
  endpoint as BENCH_RATE_FRACTION of the weaker server's capacity), falling
  back to the flat BENCH_RATE — the record says which (rate_source).
- a window whose achieved rate < BENCH_RATE_TOLERANCE × target FAILS the leg:
  a generator that can't hold the schedule has silently degraded to a closed
  loop, and those numbers must not enter the record.
- the login storm runs in its own window after the main legs drain (PBKDF2
  saturates CPU; each login invalidates the server-side auth cache).
- a generator-ceiling calibration (vegeta at max throughput against
  /System/Ping) is recorded into the summary meta; any endpoint whose target
  rate exceeds the measured ceiling fails loud.

--calibrate-rates instead measures each endpoint's max throughput on THIS
server and writes rates.json; run it against Jellyfin (the weaker side), once
per host/fixture, and commit the result next to the baseline.
"""

import argparse
import json
import math
import os
import sys
import time
from pathlib import Path

import benchlib
import bootstrap
import vegeta
from config import CONFIG, resolved_meta
from endpoints import ENDPOINTS

RAW = Path(__file__).resolve().parent / "results" / "raw"
RATES_FILE = Path(__file__).resolve().parent / "rates.json"


def load_rates():
    try:
        return json.loads(RATES_FILE.read_text())
    except OSError:
        return {"_meta": {}, "rates": {}}


def rate_for(name, rates):
    """(rate, source) for one endpoint: calibrated entry else the flat default."""
    r = rates.get("rates", {}).get(name)
    if r:
        return r, "calibrated"
    return CONFIG["BENCH_RATE"], "flat-default"


def window_secs(rate):
    """Measured-window length for one endpoint: enough requests for stable
    tail percentiles rather than a flat wall-time (precision scales with
    SAMPLES; a flat 30 s at a 500/s calibrated rate collects 15k samples the
    tails don't need, ×118 endpoints ×2 servers ×N runs = hours of nothing).
    clamp(MIN_SAMPLES/rate, floor, cap) — the floor keeps a wall-clock-long
    enough window for cache/steady-state effects, the cap bounds the slow
    endpoints. Identical for both servers (derives only from the shared rate)."""
    return max(CONFIG["BENCH_MIN_WINDOW_SECS"],
               min(CONFIG["BENCH_DURATION_SECS"],
                   math.ceil(CONFIG["BENCH_MIN_SAMPLES"] / rate)))


def measure_ceiling(base, ctx):
    """I4: an upper bound the open-loop rates must stay under, measured as the
    max throughput of /System/Ping on the server under test. This is
    min(generator capacity, that server's ping capacity) — NOT a pure
    generator number (it differs per leg), hence the honest name
    ping_ceiling_rps. As a guard it is conservative-correct: any target rate
    below it is one the generator can provably dispatch."""
    ping = next(e for e in ENDPOINTS if e["name"] == "system_ping")
    targets = vegeta.build_targets(base, ping, ctx)
    records = vegeta.max_attack(targets, duration_secs=5)
    return round(len(records) / 5, 1)


def open_loop_window(base, e, ctx, rate, warmup_secs, duration_secs):
    """Warmup at the measured rate (discarded), then the measured window.

    The warmup is time-based but tier-1 promotion is call-COUNT-based, so it
    is stretched to at least BENCH_WARMUP_MIN_CALLS at this rate — a slow
    endpoint (3 req/s calibrated) would otherwise get fewer than the ~30
    calls .NET needs to promote, biasing exactly the expensive endpoints
    where JIT matters most (review finding, round 2).
    """
    targets = vegeta.build_targets(base, e, ctx)
    if e["scenario"] == "login":
        # Fresh DeviceId per request, as target data (see vegeta.build_targets).
        # 2× headroom: vegeta rounds/catches up, and wrapping the target list
        # would reuse a DeviceId (revoking that request's token mid-window).
        targets = vegeta.build_targets(base, e, ctx, count=2 * rate * duration_secs)
    if warmup_secs:
        warmup_secs = max(warmup_secs,
                          math.ceil(CONFIG["BENCH_WARMUP_MIN_CALLS"] / rate))
        vegeta.attack(targets, rate, warmup_secs)
    records = vegeta.attack(targets, rate, duration_secs)
    return vegeta.summarize(records, e["ok"], duration_secs, rate=rate)


def run_bench(target, base):
    print(f">> [{target}] bring-up", flush=True)
    # ready_ctx reuses a still-valid provisioned state (publish runs ≥2 keep
    # the scanned volume — rescanning identical media N times bought nothing
    # but wall-clock); a fresh DB invalidates the saved token and provisions
    # from scratch. run.sh removes the ctx file whenever it wipes the volume.
    ctx = bootstrap.ready_ctx(target, base)

    rates = load_rates()
    # H1, two-stage (identical on both servers so the comparison is never
    # Rust-vs-quick-JIT): a global pass promoting the mostly-shared .NET code
    # once, then a short same-endpoint top-up at the measured rate before each
    # window. Rust has no tiers; it gets the same protocol for symmetry.
    global_warmup = CONFIG["BENCH_GLOBAL_WARMUP_SECS"]
    if global_warmup:
        print(f">> [{target}] global warmup: cycling all endpoints for {global_warmup}s "
              f"(.NET tier-1 promotion of shared code)", flush=True)
        warm_deadline = time.monotonic() + global_warmup
        warm_eps = [e for e in ENDPOINTS if not e["scenario"]]
        warming = True
        while warming:
            for e in warm_eps:
                # Per request, not per pass — a slow pass would overshoot the
                # budget by its whole length (same rule phase_c documents).
                if time.monotonic() >= warm_deadline:
                    warming = False
                    break
                benchlib.fire(base, e, ctx)
    warmup = CONFIG["BENCH_WARMUP_SECS"]
    login_rate = CONFIG["BENCH_LOGIN_RATE"]
    login_secs = CONFIG["BENCH_LOGIN_DURATION_SECS"]
    tolerance = CONFIG["BENCH_RATE_TOLERANCE"]

    ceiling = measure_ceiling(base, ctx)
    print(f">> [{target}] ping ceiling: {ceiling} rps (open-loop rates must stay below; "
          f"= min(generator, this server's /System/Ping capacity))")

    out = {"target": target, "durationSec": CONFIG["BENCH_DURATION_SECS"], "endpoints": {},
           "meta": {"engine": f"vegeta {vegeta.version()}", "ping_ceiling_rps": ceiling,
                    "rates_meta": rates.get("_meta", {}), "bench_config": resolved_meta()}}
    failures = []
    main_eps = [e for e in ENDPOINTS if not e["scenario"]]
    for i, e in enumerate(main_eps, 1):
        rate, src = rate_for(e["name"], rates)
        if rate >= ceiling:
            failures.append(f"{e['name']}: target rate {rate} ≥ ping ceiling {ceiling}")
            out["endpoints"][e["name"]] = {"p50": None, "p95": None, "p99": None,
                                           "count": 0, "rps": 0, "okPct": 0,
                                           "rate_source": src, "rate_held": False}
            continue
        dur = window_secs(rate)
        row = open_loop_window(base, e, ctx, rate, warmup, dur)
        row["rate_source"] = src
        row["duration_secs"] = dur
        row["rate_held"] = row["achieved_rate"] >= tolerance * rate
        if not row["rate_held"]:
            failures.append(f"{e['name']}: achieved {row['achieved_rate']}/s of target {rate}/s")
        out["endpoints"][e["name"]] = row
        print(f"   [{i:3}/{len(main_eps)}] {e['name']:28} rate={rate:>4}/s×{dur:>2}s "
              f"p50={row['p50']} p95={row['p95']} p99={row['p99']} ok={row['okPct']}%", flush=True)

    # Login storm — its own open-loop window after the mixed legs drain.
    login = next(e for e in ENDPOINTS if e["scenario"] == "login")
    row = open_loop_window(base, login, ctx, login_rate, 0, login_secs)
    row["rate_source"] = "login"
    row["rate_held"] = row["achieved_rate"] >= tolerance * login_rate
    if not row["rate_held"]:
        failures.append(f"auth_login: achieved {row['achieved_rate']}/s of target {login_rate}/s")
    out["endpoints"]["auth_login"] = row
    print(f"   login storm: p50={row['p50']} ok={row['okPct']}%")

    (RAW / f"{target}-summary.json").write_text(json.dumps(out, indent=2) + "\n")
    print(f">> [{target}] wrote results/raw/{target}-summary.json")
    if failures:
        print(f"!! [{target}] open-loop rate NOT HELD on {len(failures)} leg(s) — "
              f"these numbers are closed-loop-contaminated and the leg fails:", file=sys.stderr)
        for f in failures:
            print(f"!!   {f}", file=sys.stderr)
        sys.exit(3)


def calibrate_rates(target, base):
    """Measure each endpoint's max closed-throughput on THIS server and write
    rates.json with BENCH_RATE_FRACTION of it. Run against the weaker server."""
    print(f">> [{target}] calibrating per-endpoint rates "
          f"(fraction {CONFIG['BENCH_RATE_FRACTION']} of measured capacity)", flush=True)
    ctx = benchlib.bring_up(base, target)
    benchlib.pick_items(base, ctx)
    benchlib.enrich_context(base, ctx)
    fraction = CONFIG["BENCH_RATE_FRACTION"]
    rates = {}
    main_eps = [e for e in ENDPOINTS if not e["scenario"]]
    for i, e in enumerate(main_eps, 1):
        targets = vegeta.build_targets(base, e, ctx)
        records = vegeta.max_attack(targets, duration_secs=8)
        ok = sum(1 for code, _ in records if code == e["ok"])
        # Capacity counts EXPECTED-STATUS responses only (review finding,
        # round 1): an endpoint that 4xx's fast would otherwise calibrate a
        # rate the real path can never hold. Zero ok responses ⇒ no entry —
        # the bench falls back to the flat rate and records rate_source.
        capacity = ok / 8
        if not ok:
            print(f"   [{i:3}/{len(main_eps)}] {e['name']:28} 0/{len(records)} expected-status — "
                  f"NOT calibrated (flat BENCH_RATE will apply)", flush=True)
            continue
        rate = max(1, round(capacity * fraction))
        rates[e["name"]] = rate
        note = "" if ok == len(records) else f"  (! only {ok}/{len(records)} expected-status)"
        print(f"   [{i:3}/{len(main_eps)}] {e['name']:28} capacity≈{capacity:7.1f}/s "
              f"→ rate {rate}/s{note}", flush=True)
    doc = {"_meta": {"calibrated_on": target, "fraction": fraction,
                     "engine": f"vegeta {vegeta.version()}",
                     "note": "rate = fraction × measured max throughput of the weaker server; "
                             "re-run compare.py --calibrate-rates on rebaseline"},
           "rates": rates}
    RATES_FILE.write_text(json.dumps(doc, indent=2) + "\n")
    print(f">> wrote {RATES_FILE.name} ({len(rates)} endpoints)")


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--target", required=True, choices=["ferrofin", "jellyfin"])
    ap.add_argument("--base", required=True)
    ap.add_argument("--calibrate-rates", action="store_true")
    args = ap.parse_args()
    if args.calibrate_rates:
        calibrate_rates(args.target, args.base)
    else:
        run_bench(args.target, args.base)


if __name__ == "__main__":
    main()
