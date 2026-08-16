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
import os
import sys
from pathlib import Path

import benchlib
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


def measure_ceiling(base, ctx):
    """I4: the generator's own throughput ceiling on this host, measured against
    the cheapest endpoint (/System/Ping). Recorded into meta; a target rate
    above it means the generator, not the server, is the bottleneck."""
    ping = next(e for e in ENDPOINTS if e["name"] == "system_ping")
    targets = vegeta.build_targets(base, ping, ctx)
    records = vegeta.max_attack(targets, duration_secs=5)
    return round(len(records) / 5, 1)


def open_loop_window(base, e, ctx, rate, warmup_secs, duration_secs):
    """Warmup at the measured rate (discarded), then the measured window."""
    targets = vegeta.build_targets(base, e, ctx)
    if e["scenario"] == "login":
        # Fresh DeviceId per request, as target data (see vegeta.build_targets).
        targets = vegeta.build_targets(base, e, ctx, count=rate * duration_secs)
    if warmup_secs:
        vegeta.attack(targets, rate, warmup_secs)
    records = vegeta.attack(targets, rate, duration_secs)
    return vegeta.summarize(records, e["ok"], duration_secs, rate=rate)


def run_bench(target, base):
    print(f">> [{target}] bring-up", flush=True)
    ctx = benchlib.bring_up(base, target)
    benchlib.pick_items(base, ctx)
    benchlib.enrich_context(base, ctx)
    RAW.mkdir(parents=True, exist_ok=True)
    (RAW / f"{target}-ctx.json").write_text(json.dumps(ctx, indent=2) + "\n")

    rates = load_rates()
    warmup = int(os.environ.get("BENCH_WARMUP_SECONDS", "10"))
    duration = CONFIG["BENCH_DURATION_SECS"]
    login_rate = CONFIG["BENCH_LOGIN_RATE"]
    login_secs = CONFIG["BENCH_LOGIN_DURATION_SECS"]
    tolerance = CONFIG["BENCH_RATE_TOLERANCE"]

    ceiling = measure_ceiling(base, ctx)
    print(f">> [{target}] generator ceiling: {ceiling} rps (open-loop rates must stay below)")

    out = {"target": target, "durationSec": duration, "endpoints": {},
           "meta": {"engine": f"vegeta {vegeta.version()}", "generator_ceiling_rps": ceiling,
                    "rates_meta": rates.get("_meta", {}), "bench_config": resolved_meta()}}
    failures = []
    main_eps = [e for e in ENDPOINTS if not e["scenario"]]
    for i, e in enumerate(main_eps, 1):
        rate, src = rate_for(e["name"], rates)
        if rate >= ceiling:
            failures.append(f"{e['name']}: target rate {rate} ≥ generator ceiling {ceiling}")
            out["endpoints"][e["name"]] = {"p50": None, "p95": None, "p99": None,
                                           "count": 0, "rps": 0, "okPct": 0,
                                           "rate_source": src, "rate_held": False}
            continue
        row = open_loop_window(base, e, ctx, rate, warmup, duration)
        row["rate_source"] = src
        row["rate_held"] = row["achieved_rate"] >= tolerance * rate
        if not row["rate_held"]:
            failures.append(f"{e['name']}: achieved {row['achieved_rate']}/s of target {rate}/s")
        out["endpoints"][e["name"]] = row
        print(f"   [{i:3}/{len(main_eps)}] {e['name']:28} rate={rate:>4}/s "
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
        ok = [1 for code, _ in records if code == e["ok"]]
        capacity = len(records) / 8
        rate = max(1, round(capacity * fraction))
        rates[e["name"]] = rate
        print(f"   [{i:3}/{len(main_eps)}] {e['name']:28} capacity≈{capacity:7.1f}/s "
              f"→ rate {rate}/s  (ok {len(ok)}/{len(records)})", flush=True)
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
