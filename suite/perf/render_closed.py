#!/usr/bin/env python3
"""Render the closed-loop legs' reports. Port of the retired
``render-phase-c.mjs``, ``render-phase-d.mjs`` and ``render-pool-sweep.mjs``,
folded into one module because they share the raw-file plumbing.

Paths are cwd-relative (``results/…``) like the .mjs renderers — the shell
scripts cd into suite/perf first.

Subcommands::

    python3 render_closed.py c <version> <vus> <duration>
        results/raw/phaseC-*.json + phaseCmem-*.json → results/phaseC-<version>.md
    python3 render_closed.py d <version>
        results/raw/phaseD-*.json → results/phaseD-<version>.md (+ stdout)
    python3 render_closed.py pool <sha> <sizes...>
        results/raw/pool-<n>-summary.json → results/pool-sweep-<sha>.json
        (+ the aggregate and worst-endpoint tables on stdout)

JELLYFIN_IMAGE env names the Jellyfin image in the phase C header.
"""

import argparse
import json
import os
from datetime import datetime, timezone
from pathlib import Path

RAW = Path("results/raw")


def read(name):
    """A raw JSON file, or None when the leg didn't run (BENCH_ONLY)."""
    p = RAW / name
    return json.loads(p.read_text()) if p.exists() else None


def fmt(x):
    """Render a number the way JS template interpolation did: no trailing .0."""
    if isinstance(x, float) and x.is_integer():
        return str(int(x))
    return str(x)


def n(x):
    return "·" if x is None else fmt(x)


# ── phase C ──────────────────────────────────────────────────────────────────

def mib(b):
    return "·" if b is None else f"{int(b / 1048576 + 0.5)} MiB"


def cell(e):
    if e and e.get("p50") is not None:
        return f"{n(e['p50'])} / {n(e['p95'])} / {n(e['p99'])}"
    return "·"


def okc(h, j):
    return (f"{fmt(h['okPct']) if h else '·'}% / "
            f"{fmt(j['okPct']) if j else '·'}%")


def render_c(version, vus, dur):
    """Phase C report: mixed-load latencies (contention) + memory footprint."""
    H, J = read("phaseC-ferrofin.json"), read("phaseC-jellyfin.json")
    Hm, Jm = read("phaseCmem-ferrofin.json"), read("phaseCmem-jellyfin.json")
    he = (H or {}).get("endpoints") or {}
    je = (J or {}).get("endpoints") or {}
    names = sorted(set(he) | set(je))

    rows = []
    for name in names:
        h, j = he.get(name), je.get(name)
        spd = ((j["p50"] / h["p50"])
               if h and j and h.get("p50") and j.get("p50") else None)
        rows.append(f"| `{name}` | {cell(h)} | {cell(j)} | {okc(h, j)} | "
                    f"{'·' if spd is None else f'{spd:.2f}×'} |")

    md = f"""# Ferrofin vs Jellyfin — Phase C (mixed contention + memory footprint)

- **Ferrofin:** `{version}`  **Jellyfin:** `{os.environ.get('JELLYFIN_IMAGE') or '?'}`
- **Mixed load:** all endpoints hit concurrently, {vus} VUs × {dur} (closed loop).
- This is the CONTENTION view — every endpoint's latency here includes
  interference from the others (shared DB pool, locks). Use Phase A for each
  endpoint's own cost; use this to see how a heavy endpoint drags the rest.

## Footprint (whole run, cgroup-accounted)

| Metric | Ferrofin | Jellyfin |
|---|---|---|
| memory.peak (high-water, incl. scan) | {mib((Hm or {}).get('mem_peak'))} | {mib((Jm or {}).get('mem_peak'))} |
| anon working set (end of load) | {mib((Hm or {}).get('mem_anon'))} | {mib((Jm or {}).get('mem_anon'))} |

## Mixed-load latency (ms, p50 / p95 / p99)

| Endpoint | Ferrofin | Jellyfin | 200-rate (H / J) | p50 speedup |
|---|---|---|---|---|
{chr(10).join(rows)}

> Latencies here are inflated by cross-endpoint contention by design; a slow
> endpoint (e.g. one that saturates the DB pool) raises the whole column.
"""
    Path(f"results/phaseC-{version}.md").write_text(md)


# ── phase D ──────────────────────────────────────────────────────────────────

def render_d(version):
    """Phase D comparison table; also echoed to stdout like the .mjs did."""
    h, j = read("phaseD-ferrofin.json"), read("phaseD-jellyfin.json")

    def step_cell(s):
        return f"{fmt(s['p50'])} / {fmt(s['p95'])} / {fmt(s['p99'])}" if s else "·"

    def sess_cell(t):
        s = (t or {}).get("sessions")
        return f"{fmt(s['p50'])} (n={s['count']})" if s else "·"

    steps = ["home", "library", "detail", "images", "playback"]
    md = f"""# Phase D — realistic load ({os.environ.get('PHASE_D_VUS') or 8} clients, think time)

- **Ferrofin:** `{version}` · window {os.environ.get('PHASE_D_DUR') or '120s'}
- Every VU is its own logged-in device running home → browse → detail → posters → playback
  (incl. the playstate write path), with 1–3 s think time. p50/p95/p99 in ms per step.

| Step | Ferrofin | Jellyfin |
|---|---|---|
"""
    for s in steps:
        md += (f"| {s} | {step_cell((h or {}).get('steps', {}).get(s))} "
               f"| {step_cell((j or {}).get('steps', {}).get(s))} |\n")
    md += f"| whole session (ms) | {sess_cell(h)} | {sess_cell(j)} |\n"

    def ok(t):
        v = (t or {}).get("okPct")
        return "·" if v is None else fmt(v)

    md += f"| non-4xx/5xx rate | {ok(h)}% | {ok(j)}% |\n"

    Path(f"results/phaseD-{version}.md").write_text(md)
    print(md)


# ── pool sweep ───────────────────────────────────────────────────────────────

def med(xs):
    """The .mjs median: sort the non-nulls, take the upper-middle element."""
    s = sorted(x for x in xs if x is not None)
    return s[len(s) // 2] if s else None


def render_pool(sha, pools):
    """Aggregate the per-size summaries into one sweep record + two tables."""
    runs = []
    for p in pools:
        r = json.loads((RAW / f"pool-{p}-summary.json").read_text())
        # {pool:+p, ...file} in the JS: the file's own pool key wins.
        runs.append({"pool": int(p), **r})

    record = {
        "sha": sha,
        "when": datetime.now(timezone.utc).isoformat(timespec="milliseconds")
                .replace("+00:00", "Z"),
        "vus": os.environ.get("BENCH_VUS") or "50",
        "duration": os.environ.get("BENCH_DURATION") or "30s",
        "runs": [],
    }
    for r in runs:
        eps = list(r["endpoints"].values())
        total_rps = round(sum(e.get("rps") or 0 for e in eps), 1)
        record["runs"].append({
            "pool": r["pool"],
            "endpoints": r["endpoints"],
            "aggregate": {
                "med_p50": med([e["p50"] for e in eps]),
                "med_p95": med([e["p95"] for e in eps]),
                "med_p99": med([e["p99"] for e in eps]),
                "total_rps": int(total_rps) if total_rps.is_integer() else total_rps,
                "errored": sum(1 for e in eps if e["okPct"] < 100),
            },
        })
    Path(f"results/pool-sweep-{sha}.json").write_text(json.dumps(record, indent=2))

    # Aggregate table.
    print("\npool | med p50 | med p95 | med p99 | total rps | errored eps")
    print("-----|---------|---------|---------|-----------|------------")
    for r in record["runs"]:
        a = r["aggregate"]
        null = lambda x: "null" if x is None else fmt(x)  # noqa: E731 — JS printed literal null
        print(f"{str(r['pool']).rjust(4)} | {null(a['med_p50']).rjust(7)} | "
              f"{null(a['med_p95']).rjust(7)} | {null(a['med_p99']).rjust(7)} | "
              f"{null(a['total_rps']).rjust(9)} | {a['errored']}")

    # Worst-endpoint drilldown: the 8 slowest endpoints at pool=min, across sizes.
    base = record["runs"][0]
    worst = [name for name, v in sorted(
        ((k, v) for k, v in base["endpoints"].items() if v["p50"] is not None),
        key=lambda kv: kv[1]["p50"], reverse=True)][:8]
    print(f"\nendpoint p50 by pool size (8 slowest at pool={base['pool']}):")
    print(" | ".join(["endpoint"] + [f"p={r['pool']}" for r in record["runs"]]))
    for name in worst:
        print(" | ".join(
            [name] + [n((r["endpoints"].get(name) or {}).get("p50"))
                      for r in record["runs"]]))
    print(f"\nwrote results/pool-sweep-{sha}.json")


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    sub = ap.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("c", help="phase C report")
    c.add_argument("version")
    c.add_argument("vus")
    c.add_argument("duration")
    d = sub.add_parser("d", help="phase D report")
    d.add_argument("version")
    pool = sub.add_parser("pool", help="pool-sweep record + tables")
    pool.add_argument("sha")
    pool.add_argument("sizes", nargs="+")
    args = ap.parse_args()
    if args.cmd == "c":
        render_c(args.version, args.vus, args.duration)
    elif args.cmd == "d":
        render_d(args.version)
    else:
        render_pool(args.sha, args.sizes)


if __name__ == "__main__":
    main()
