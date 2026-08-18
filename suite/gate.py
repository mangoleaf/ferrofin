#!/usr/bin/env python3
"""suite/gate.py — THE regression gate: the single comparator and single baseline.

One place computes "regressed = ANY of p50/p95/p99 > factor × baseline, or
200-rate < 100%" (the all-three-percentiles rule is a repo-owner hard
requirement — median-only gating hides tail regressions). Two input shapes,
one baseline file (this directory's perf-baseline.json, sections `raw` and
`merged`):

Raw capture mode — driven by suite/perf/perf-gate.sh, which runs perf_gate.py
(open-loop vegeta) per sentinel endpoint into
results/raw/perfgate-ferrofin-<name>.json (CWD-relative, the runner cd's into
suite/perf/):

  python3 ../gate.py compare-raw    <baselineFile> <factor> <name...>
  python3 ../gate.py rebaseline-raw <baselineFile> <rate> <secs> <name...>

compare-raw prints a before/after table (all three percentiles) to STDERR and
the space-separated regressed endpoint names to STDOUT — the runner re-runs
just those once to rule out short-window noise. Exit 0 normally; exit 2 only
on hard errors (missing baseline/section) so the shell can distinguish
"regression" (names on stdout) from "couldn't run" and never silently pass.

Merged record mode — reads the latest merged suite run (results/runs.json)
and additionally fails when an op that was `deep_verified` in the baseline has
regressed to unverified (parity and perf gate each other):

  python3 suite/gate.py               check latest run vs baseline
  python3 suite/gate.py --rebaseline  write the merged baseline from the latest run
"""
import json
import os
import sys
from pathlib import Path

RESULTS = Path(__file__).resolve().parent / "results"
BASELINE = Path(__file__).resolve().parent / "perf-baseline.json"

# The suite's methodology knobs (default < bench.conf < env — see config.py).
sys.path.insert(0, str(Path(__file__).resolve().parent / "perf"))
from config import CONFIG  # noqa: E402

FACTOR = float(os.environ.get("PERF_GATE_FACTOR") or CONFIG["PERF_GATE_FACTOR"])
# Absolute jitter floor (ms): a percentile trip additionally needs this much
# real worsening — sub-ms endpoints can "2× regress" by 1 ms of OS noise.
# PERF_GATE_MIN_DELTA_MS env remains as a gate-only override.
MIN_DELTA_MS = float(os.environ.get("PERF_GATE_MIN_DELTA_MS")
                     or CONFIG["BENCH_NOISE_FLOOR_MS"])
PCTS = ("p50", "p95", "p99")


def classify(base, cur, factor, min_delta_ms=0.0):
    """Which checks trip for one endpoint: [] = ok.

    `cur` needs {p50,p95,p99,ok,bad}; no result / no measured 200s ⇒
    ['nodata']. A percentile trips on strictly-greater than factor× (equal is
    not a regression) AND an absolute worsening above `min_delta_ms` — the
    jitter floor: on sub-millisecond endpoints (image serving at ~1 ms p95)
    the tail is OS/page-cache noise and a 2× "ratio regression" can be a 1 ms
    delta, which no user can feel and no code change caused (observed on the
    first clean-HEAD stability runs, plan 08 step 2). The floor never masks a
    real regression: any human-visible slowdown clears a few ms easily. A
    zero baseline with a current above the floor is treated as Infinity.
    Any non-200 ⇒ '200%'.
    """
    if not cur or not cur.get("ok"):
        return ["nodata"]
    tripped = []
    for p in PCTS:
        b, c = base[p], cur[p]
        ratio = (c / b) if b > 0 else (float("inf") if c > 0 else 1.0)
        if ratio > factor and (c - b) > min_delta_ms:
            tripped.append(p)
    if cur.get("bad", 0) > 0:
        tripped.append("200%")
    return tripped


# ── raw capture mode (suite/perf/perf-gate.sh) ────────────────────────────────

def _load_raw(name):
    try:
        return json.loads(Path(f"results/raw/perfgate-ferrofin-{name}.json").read_text())
    except (OSError, ValueError):
        return None


def _read_baseline_file(path):
    try:
        return json.loads(Path(path).read_text())
    except (OSError, ValueError):
        return {}


def rebaseline_raw(baseline_file, rate, secs, names):
    """Writes the `raw` section from the current captures, preserving `merged`."""
    endpoints = {}
    for name in names:
        cur = _load_raw(name)
        if not cur or not cur.get("ok"):
            sys.exit(f"rebaseline: no data for {name} — aborting")
        if cur.get("bad"):
            sys.exit(f"rebaseline: {name} had {cur['bad']} non-200s — refusing to baseline a broken endpoint")
        if cur.get("rate_held") is False:
            sys.exit(f"rebaseline: {name} did not hold its open-loop rate — refusing to baseline a degraded window")
        endpoints[name] = {p: cur[p] for p in PCTS}
    doc = _read_baseline_file(baseline_file)
    doc["raw"] = {"params": {"rate": int(rate), "secs": int(secs)}, "endpoints": endpoints}
    Path(baseline_file).write_text(json.dumps(doc, indent=2) + "\n")
    print(f"baselined {len(names)} endpoints @ {rate}/s × {secs}s → {baseline_file} [raw]",
          file=sys.stderr)


def compare_raw(baseline_file, factor, names):
    doc = _read_baseline_file(baseline_file)
    raw = doc.get("raw")
    if not raw:
        print(f"perf-gate: no raw baseline in {baseline_file} — run `./perf-gate.sh --rebaseline` first",
              file=sys.stderr)
        sys.exit(2)
    bp = raw.get("params", {})
    err = sys.stderr
    fmt = lambda n: "—" if n is None else f"{n:.1f}"  # noqa: E731 — tiny table formatter
    if "rate" not in bp:
        print("perf-gate: baseline predates the open-loop migration (captured with "
              "closed-loop VUs) — numbers are methodology-incomparable; run "
              "./perf-gate.sh --rebaseline once", file=err)
        sys.exit(2)
    print(f"perf-gate: factor {factor}×, baseline @ {bp['rate']}/s × {bp.get('secs', '?')}s", file=err)
    print("endpoint".ljust(24) + "".join(f"{p} base→cur (×)".ljust(22) for p in PCTS) + "200%  verdict", file=err)

    regressed = []
    for name in names:
        base = raw.get("endpoints", {}).get(name)
        cur = _load_raw(name)
        if not cur or not cur.get("ok"):
            regressed.append(name)
            print(name.ljust(24) + "NO DATA (no measured expected-status responses)", file=err)
            continue
        if cur.get("rate_held") is False:
            # The window degraded into a closed loop (generator couldn't hold
            # the schedule) — its percentiles are not comparable to an
            # open-loop baseline. Counted as a failure so the runner's
            # retry-once path re-measures it.
            regressed.append(name)
            print(name.ljust(24) + "RATE NOT HELD (open-loop window degraded — remeasure)", file=err)
            continue
        if not base:
            print(name.ljust(24) + f"{fmt(cur['p50'])}/{fmt(cur['p95'])}/{fmt(cur['p99'])}   (no baseline — skipped)",
                  file=err)
            continue
        tripped = classify(base, cur, factor, MIN_DELTA_MS)
        rate200 = f"{100 * cur['ok'] / (cur['ok'] + cur['bad']):.0f}%"
        cols = ""
        for p in PCTS:
            ratio = (cur[p] / base[p]) if base[p] > 0 else float("inf")
            cols += f"{fmt(base[p])}→{fmt(cur[p])} ({ratio:.2f}{'!' if ratio > factor else ''})".ljust(22)
        print(name.ljust(24) + cols + rate200.ljust(6) + (f"FAIL {','.join(tripped)}" if tripped else "ok"),
              file=err)
        if tripped:
            regressed.append(name)
    sys.stdout.write(" ".join(regressed))


# ── merged record mode (suite/run.sh gate) ───────────────────────────────────

def latest_run():
    runs = json.loads((RESULTS / "runs.json").read_text())["runs"]
    live = [r for r in runs if not r["meta"].get("legacy")]
    if not live:
        sys.exit("gate: no non-legacy run in suite/results/runs.json — run `suite/run.sh all` first")
    return live[-1]


def _run_model(run):
    return (run["meta"].get("load") or {}).get("model")


def rebaseline_merged(run):
    # A baseline is only meaningful for the methodology that will be gated
    # against it — stamping open-loop unconditionally would disarm the
    # refusal guard the moment someone rebaselines from a legacy record
    # (review finding, round 1). Derive from the run; refuse anything else.
    model = _run_model(run)
    if model != "open-loop":
        sys.exit(f"gate: refusing to baseline a non-open-loop run (meta.load.model={model!r}) "
                 "— produce a run with the current suite (`suite/run.sh all`) first")
    # Keyed by VARIANT (each /Items variant has its own latency); deep_verified is its op's.
    variants = {}
    for o in run["operations"]:
        p = o["perf"]
        if p["f_p50"] is None:
            continue
        variants[p["variant"]] = {"op": o["op"], "f_p50": p["f_p50"], "f_p95": p["f_p95"],
                                  "f_p99": p["f_p99"], "deep_verified": o["parity"]["deep_verified"]}
        # H2: cold sentinels carry their fresh-process first-request latency —
        # gated separately from warm (cold-vs-cold only, gross regressions).
        if (p.get("cold") or {}).get("f_first") is not None:
            variants[p["variant"]]["f_cold_first"] = p["cold"]["f_first"]
    doc = _read_baseline_file(BASELINE)
    doc["merged"] = {"factor": FACTOR, "engine": model,
                     "ferrofin": run["meta"]["ferrofin"], "variants": variants}
    BASELINE.write_text(json.dumps(doc, indent=2) + "\n")
    print(f">> wrote {BASELINE.name}: {len(variants)} variants baselined at {run['meta']['ferrofin']} [merged]")


def check_merged(run):
    if _run_model(run) != "open-loop":
        sys.exit(f"gate: latest run is not an open-loop record (meta.load.model="
                 f"{_run_model(run)!r}) — methodology-incomparable with the baseline")
    doc = _read_baseline_file(BASELINE)
    merged = doc.get("merged")
    if not merged:
        sys.exit("gate: no merged baseline — run `suite/run.sh gate --rebaseline` once to establish one")
    if merged.get("engine") != "open-loop":
        sys.exit("gate: merged baseline predates the open-loop migration — "
                 "methodology-incomparable; run `suite/run.sh gate --rebaseline` once")
    base = merged["variants"]
    fails = []
    seen = set()
    for o in run["operations"]:
        op, p, par = o["op"], o["perf"], o["parity"]
        b = base.get(p["variant"])
        if b is None:
            continue
        seen.add(p["variant"])
        if p["f_ok"] is not None and p["f_ok"] < 100:
            fails.append(f"{p['variant']}: Ferrofin 200-rate {p['f_ok']}% < 100%")
        for pct in ("f_p50", "f_p95", "f_p99"):
            if (p[pct] is not None and b[pct] and p[pct] > b[pct] * FACTOR
                    and (p[pct] - b[pct]) > MIN_DELTA_MS):
                fails.append(f"{p['variant']} {pct}: {p[pct]} > {b[pct]}×{FACTOR} (={round(b[pct] * FACTOR, 1)})")
        if b.get("deep_verified") and not par["deep_verified"]:
            fails.append(f"{op} ({p['variant']}): parity regressed — was deep_verified, now "
                         f"{par['depth']}/unverified")
        # Cold gates cold-vs-cold only (never against warm): same gross-factor
        # rule; cold first-requests are high-variance, so only a clear breach
        # (factor AND absolute delta) fails.
        cold_now = (p.get("cold") or {}).get("f_first")
        cold_base = b.get("f_cold_first")
        if (cold_now is not None and cold_base and cold_now > cold_base * FACTOR
                and (cold_now - cold_base) > MIN_DELTA_MS):
            fails.append(f"{p['variant']} cold_first: {cold_now} > {cold_base}×{FACTOR} "
                         f"(={round(cold_base * FACTOR, 1)})")

    # A baselined variant that vanished from the run is a silent coverage hole
    # — the exact class the fail-loud manifest exists for (review, round 2).
    vanished = sorted(set(base) - seen)
    if vanished:
        fails.append(f"baselined variants absent from this run: {', '.join(vanished)}")

    if fails:
        print(f"PERF/PARITY GATE FAILED ({len(fails)} regressions vs baseline):", file=sys.stderr)
        for f in fails:
            print(f"  ✗ {f}", file=sys.stderr)
        sys.exit(1)
    print(f">> gate OK: {len(base)} baselined ops within {FACTOR}× and parity held "
          f"(run {run['meta']['ferrofin']})")


if __name__ == "__main__":
    argv = sys.argv[1:]
    if argv[:1] == ["compare-raw"]:
        compare_raw(argv[1], float(argv[2]), argv[3:])
    elif argv[:1] == ["rebaseline-raw"]:
        rebaseline_raw(argv[1], argv[2], argv[3], argv[4:])
    elif "--rebaseline" in argv:
        rebaseline_merged(latest_run())
    else:
        check_merged(latest_run())
