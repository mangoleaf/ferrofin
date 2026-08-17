#!/usr/bin/env python3
"""suite/aggregate.py — reduce N same-SHA runs to ONE publishable record (C1/C2).

A single run's numbers are point estimates: the k6-era variance study showed
the headline median-speedup swinging 2.28× between runs of IDENTICAL code
(~30% noise), so published records are distributions over BENCH_RUNS
independent runs — per endpoint and per side, median ± IQR for p50/p95/p99 —
and the speedup RATIO is demoted to a caveated footnote (median-of-ratios
cannot be rescued by more runs at reasonable cost; the fix is to stop leading
with it, not to average it harder).

    python3 suite/aggregate.py [<sha>]     # default: the newest run's SHA

Reads the N runs for that SHA out of suite/results/runs.json (merge.py's
rerun accumulation), writes suite/results/agg-<sha>.json, upserts it into
suite/results/aggregates.json (the viewer's aggregate feed), and renders
suite/results/agg-<sha>.md — the publishable summary, which leads with
parity coverage and per-endpoint absolute latency distributions.
"""

import json
import sys
from pathlib import Path
from statistics import median, quantiles

RESULTS = Path(__file__).resolve().parent / "results"

# The noise floor (D1) applies to the paired ratios here too — see paired_speedup.
sys.path.insert(0, str(Path(__file__).resolve().parent / "perf"))
from config import CONFIG  # noqa: E402

# Per-endpoint warm metrics aggregated across runs, per side.
METRICS = ("p50", "p95", "p99")


def paired_speedup_of(h_med, j_med, floor):
    """(ratio | None, is_tie) — the paired per-endpoint statistic: ratio of
    the two MEDIANS (stable numerator/denominator, never a median of noisy
    per-run ratios), with the noise floor applied: a sub-floor delta carries
    no ratio at all (division would amplify jitter into fake multiples — the
    k6-era aggregate had a "391×" from a sub-ms endpoint the verdict logic
    itself called a tie). Pure so aggregate_selftest.py can pin the rule."""
    if h_med is None or j_med is None:
        return None, False
    # Tie check BEFORE the zero check so every measured pair classifies into a
    # bucket (review, round 3): a zero median with a sub-floor counterpart is
    # a tie; with a floor-clearing counterpart the ratio is undefined
    # (division by zero) and stays out — unreachable at ms rounding anyway.
    if abs(h_med - j_med) < floor:
        return None, True
    if not h_med:
        return None, False
    return round(j_med / h_med, 2), False


def dist(values):
    """{med, iqr, min, max, n} over the runs' values (None-filtered).
    IQR needs ≥2 points; with one run it is 0 by definition (say so via n)."""
    vals = [v for v in values if v is not None]
    if not vals:
        return None
    if len(vals) == 1:
        return {"med": vals[0], "iqr": 0.0, "min": vals[0], "max": vals[0], "n": 1}
    q = quantiles(vals, n=4, method="inclusive")
    return {"med": round(median(vals), 2), "iqr": round(q[2] - q[0], 2),
            "min": min(vals), "max": max(vals), "n": len(vals)}


def aggregate(runs):
    """One aggregate document from N same-SHA run records."""
    by_variant = {}
    for r in runs:
        for o in r["operations"]:
            v = by_variant.setdefault(o["perf"]["variant"], {
                "op": o["op"], "owner": o.get("owner", "core"),
                "deep_verified": o["parity"]["deep_verified"], "runs": []})
            v["runs"].append(o["perf"])

    endpoints = {}
    for variant, v in sorted(by_variant.items()):
        rows = v["runs"]
        ep = {"op": v["op"], "owner": v["owner"],
              "n_runs": len(rows),
              "n_comparable": sum(1 for p in rows if p.get("comparable"))}
        for side in ("h", "j"):
            for m in METRICS:
                ep[f"{side}_{m}"] = dist([p.get(f"{side}_{m}") for p in rows])
        # C2 + D1: see paired_speedup_of — sub-floor pairs get NO ratio.
        h50, j50 = ep["h_p50"], ep["j_p50"]
        ep["paired_speedup"], is_tie = paired_speedup_of(
            h50 and h50["med"], j50 and j50["med"], CONFIG["BENCH_NOISE_FLOOR_MS"])
        if is_tie:
            ep["paired_tie"] = True
        cold_firsts_h = [((p.get("cold") or {}).get("h_first")) for p in rows]
        cold_firsts_j = [((p.get("cold") or {}).get("j_first")) for p in rows]
        if any(x is not None for x in cold_firsts_h + cold_firsts_j):
            ep["cold_first"] = {"h": dist(cold_firsts_h), "j": dist(cold_firsts_j)}
        endpoints[variant] = ep

    heads = [r["headline"] for r in runs]
    paired = [ep["paired_speedup"] for ep in endpoints.values()
              if ep["paired_speedup"] is not None and ep["n_comparable"] == len(runs)]
    paired_ties = sum(1 for ep in endpoints.values() if ep.get("paired_tie"))
    headline = {
        # The honest headline: exact and deterministic.
        "parity_coverage": heads[-1].get("parity_coverage"),
        "comparable_rows": dist([h.get("comparable_rows") for h in heads]),
        "win_rate": dist([h.get("win_rate") for h in heads]),
        "ties": dist([h.get("ties") for h in heads]),
        # Distribution of paired per-endpoint speedups over always-comparable
        # endpoints — the defensible single number, WITH its spread.
        # paired_excluded_ties = endpoints whose medians differ by less than
        # the noise floor: they carry no ratio at all (division would amplify
        # jitter into fake multiples) and are counted here instead.
        "paired_speedup": dist(paired),
        "paired_excluded_ties": paired_ties,
        "noise_floor_ms": CONFIG["BENCH_NOISE_FLOOR_MS"],
        # Footnote only — see the module docstring.
        "median_speedup_footnote": {
            "values": [h.get("median_speedup") for h in heads],
            "caveat": "median-of-ratios; high run-to-run variance (CV≈30% measured "
                      "on identical code) — read the per-endpoint distributions instead",
        },
        "owners": heads[-1].get("owners"),
    }
    meta = dict(runs[-1]["meta"])
    meta["aggregated_runs"] = len(runs)
    meta["run_seqs"] = [r["meta"].get("run_seq", 1) for r in runs]
    return {"meta": meta, "headline": headline, "endpoints": endpoints}


def fmt_dist(d):
    return "—" if not d else f"{d['med']}±{d['iqr']}"


def render_md(agg, sha):
    m, h = agg["meta"], agg["headline"]
    out = [f"# Ferrofin vs Jellyfin — aggregate of {m['aggregated_runs']} runs @ `{m.get('ferrofin', sha)}`\n"]
    out.append(f"- **Jellyfin:** `{m.get('jellyfin_image')}` · **engine:** {m.get('engine')} "
               f"· **when:** {m.get('when')}")
    out.append(f"- **Parity coverage (exact): {h['parity_coverage']}** · "
               f"comparable rows {fmt_dist(h['comparable_rows'])} · "
               f"win-rate {fmt_dist(h['win_rate'])} · ties {fmt_dist(h['ties'])}")
    out.append(f"- Paired speedup over always-comparable endpoints: **{fmt_dist(h['paired_speedup'])}** "
               f"(ratio of per-endpoint medians; spread is IQR across endpoints; "
               f"{h['paired_excluded_ties']} endpoint(s) tied under the {h['noise_floor_ms']} ms "
               f"floor carry no ratio)\n")
    out.append("Latency is ms, shown as `median±IQR` across runs. Cold is a fresh-process "
               "first request — separate from warm, never blended.\n")
    out.append("| endpoint | owner | H p50 | J p50 | H p95 | J p95 | H p99 | J p99 | paired | cold H/J (first) | n |")
    out.append("|---|---|---|---|---|---|---|---|---|---|---|")
    for variant, ep in agg["endpoints"].items():
        cold = ep.get("cold_first")
        cold_s = f"{fmt_dist(cold['h'])} / {fmt_dist(cold['j'])}" if cold else "—"
        out.append(f"| `{variant}` | {ep['owner']} | {fmt_dist(ep['h_p50'])} | {fmt_dist(ep['j_p50'])} "
                   f"| {fmt_dist(ep['h_p95'])} | {fmt_dist(ep['j_p95'])} "
                   f"| {fmt_dist(ep['h_p99'])} | {fmt_dist(ep['j_p99'])} "
                   f"| {ep['paired_speedup'] or '—'}× | {cold_s} | {ep['n_comparable']}/{ep['n_runs']} |")
    fn = h["median_speedup_footnote"]
    out.append(f"\n> Footnote — headline `median_speedup` per run was {fn['values']}: {fn['caveat']}.\n")
    return "\n".join(out)


def main():
    runs_doc = json.loads((RESULTS / "runs.json").read_text())["runs"]
    # Aggregates are published records: only non-legacy, OPEN-LOOP runs may
    # enter one (the gate refuses pre-migration baselines; the aggregate feed
    # must not publish pre-migration numbers either — review finding, round 1).
    live = [r for r in runs_doc if not r["meta"].get("legacy")
            and (r["meta"].get("load") or {}).get("model") == "open-loop"]
    if not live:
        sys.exit("aggregate: no open-loop runs in runs.json — run `suite/run.sh publish` first")
    sha = sys.argv[1] if len(sys.argv) > 1 else live[-1]["meta"]["ferrofin_sha"]
    same = [r for r in live if r["meta"]["ferrofin_sha"] == sha]
    if not same:
        sys.exit(f"aggregate: no open-loop runs for SHA {sha!r}")

    agg = aggregate(same)
    out = RESULTS / f"agg-{sha}.json"
    out.write_text(json.dumps(agg, indent=2) + "\n")
    (RESULTS / f"agg-{sha}.md").write_text(render_md(agg, sha) + "\n")

    # The viewer's aggregate feed: one entry per SHA, newest last.
    feed_path = RESULTS / "aggregates.json"
    try:
        feed = json.loads(feed_path.read_text())
    except OSError:
        feed = {"aggregates": []}
    feed["aggregates"] = [a for a in feed["aggregates"]
                          if a["meta"]["ferrofin_sha"] != sha] + [agg]
    feed_path.write_text(json.dumps(feed, indent=2) + "\n")

    n = agg["meta"]["aggregated_runs"]
    h = agg["headline"]
    print(f">> wrote {out.name} + agg-{sha}.md ({n} runs aggregated)")
    print(f"   parity {h['parity_coverage']} · paired speedup {fmt_dist(h['paired_speedup'])} "
          f"· win-rate {fmt_dist(h['win_rate'])}")
    if n < 2:
        print("   NOTE: single run — IQRs are 0 by definition; a publishable record "
              "wants BENCH_RUNS runs (suite/run.sh publish)")


if __name__ == "__main__":
    main()
