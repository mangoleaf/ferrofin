#!/usr/bin/env python3
"""Render the Phase A / Phase B markdown reports from results/raw JSON.

Faithful port of render-phase-a.mjs + render-phase-b.mjs (retired with the
k6/node toolchain), merged into one stdlib script with two subcommands:

    python3 render_phases.py a <version> <rate> <dur>
        reads  results/raw/phaseA-<target>-<endpoint>.json
        writes results/phaseA-<version>.md
    python3 render_phases.py b <version>
        reads  results/raw/phaseBmax-<target>-<endpoint>.json
        writes results/phaseB-<version>.md

Same input files, same output filenames, same table columns as the .mjs
renderers, so downstream readers (README benchmark section, release notes)
see no format change. JELLYFIN_IMAGE comes from the environment for the
header line, exactly as before.
"""

import json
import os
import re
import sys
from pathlib import Path

RAW = Path("results/raw")


def load(prefix, target):
    """{endpoint: record} for every results/raw/<prefix>-<target>-*.json."""
    out = {}
    if not RAW.is_dir():
        return out
    pat = re.compile(rf"^{prefix}-{target}-(.+)\.json$")
    for f in sorted(RAW.iterdir()):
        m = pat.match(f.name)
        if m:
            out[m.group(1)] = json.loads(f.read_text())
    return out


def n(x, d=2):
    """Number to `d` decimals, or the '·' placeholder for a missing value."""
    return "·" if x is None else f"{float(x):.{d}f}"


def spd(h, j):
    """Jellyfin/Ferrofin ratio (>1 ⇒ Ferrofin faster / leaner), or None."""
    return (j / h) if h and j else None


def jellyfin_image():
    """The Jellyfin image tag for the header, from the run env."""
    return os.environ.get("JELLYFIN_IMAGE") or "?"


def render_a(version, rate, dur):
    """Phase A: per-endpoint latency + CPU-per-request comparison table."""
    H, J = load("phaseA", "ferrofin"), load("phaseA", "jellyfin")
    names = sorted(set(H) | set(J))

    def cell(e):
        return f"{n(e.get('p50'))} / {n(e.get('p95'))} / {n(e.get('p99'))}" if e else "·"

    def drop(e):
        return f" ⚠️{e['dropped']}" if e and e.get("dropped", 0) > 0 else ""

    rows = []
    for name in names:
        h, j = H.get(name), J.get(name)
        lat_spd = spd(h and h.get("p50"), j and j.get("p50"))
        # >1 ⇒ Ferrofin uses less CPU.
        cpu_spd = spd(h and h.get("cpu_us_per_req"), j and j.get("cpu_us_per_req"))
        rows.append(
            f"| `{name}` | {cell(h)}{drop(h)} | {cell(j)}{drop(j)} "
            f"| {n(h and h.get('cpu_us_per_req'), 1)} / {n(j and j.get('cpu_us_per_req'), 1)} "
            f"| {'·' if lat_spd is None else n(lat_spd) + 'x'} "
            f"| {'·' if cpu_spd is None else n(cpu_spd) + 'x'} |")
    table = "\n".join(rows)

    md = f"""# Ferrofin vs Jellyfin — Phase A (isolated, open-model per endpoint)

- **Ferrofin:** `{version}`  **Jellyfin:** `{jellyfin_image()}`
- **Model:** open (constant arrival rate), one endpoint at a time, {rate} req/s for {dur} after warm-up.
- **CPU/req:** container cgroup `cpu.stat usage_usec` delta over the run, minus idle baseline, ÷ requests.
- Isolated ⇒ each row is that handler's own latency + CPU, with no cross-endpoint contention.

## Latency (ms, p50 / p95 / p99) and CPU cost

| Endpoint | Ferrofin lat | Jellyfin lat | CPU µs/req (H / J) | lat speedup | CPU efficiency |
|---|---|---|---|---|---|
{table}

> "lat speedup" = Jellyfin p50 ÷ Ferrofin p50 (>1 = Ferrofin faster). "CPU efficiency" =
> Jellyfin µs/req ÷ Ferrofin µs/req (>1 = Ferrofin burns less CPU per request). ⚠️N = N
> dropped arrivals (endpoint could not sustain the offered rate ⇒ treat its row as
> saturated, not a clean latency).
"""
    Path(f"results/phaseA-{version}.md").write_text(md)


def render_b(version):
    """Phase B: per-endpoint max sustainable RPS comparison table."""
    H, J = load("phaseBmax", "ferrofin"), load("phaseBmax", "jellyfin")
    names = sorted(set(H) | set(J))

    def num(x):
        return "·" if x is None else x

    rows = []
    knee_ms = None
    for name in names:
        h, j = H.get(name), J.get(name)
        hmax = h.get("max_rps") if h else None
        jmax = j.get("max_rps") if j else None
        ratio = (hmax / jmax) if hmax and jmax else None
        knee_ms = knee_ms or (h or j or {}).get("knee_p99_ms")
        rows.append(
            f"| `{name}` | {num(hmax)} | {num(jmax)} "
            f"| {num(h.get('knee_rate') if h else None)} / {num(j.get('knee_rate') if j else None)} "
            f"| {num(h.get('p99_at_max') if h else None)} / {num(j.get('p99_at_max') if j else None)} "
            f"| {'·' if ratio is None else f'{ratio:.2f}×'} |")
    table = "\n".join(rows)

    md = f"""# Ferrofin vs Jellyfin — Phase B (per-endpoint saturation sweep)

- **Ferrofin:** `{version}`  **Jellyfin:** `{jellyfin_image()}`
- Each endpoint driven (open model) at a rising arrival-rate ladder until the
  server drops arrivals or stops returning 200; the last clean rate is its
  **max sustainable throughput** (req/s). The **knee** is the lowest rate at
  which p99 exceeded {num(knee_ms)} ms — where latency departs, usually well
  before hard saturation, and the number users would feel first. Curated
  endpoint subset. Deliberately a separate report from the fixed-rate latency
  comparison: max-throughput and latency must never share a headline.

## Max sustainable throughput (req/s)

| Endpoint | Ferrofin max RPS | Jellyfin max RPS | knee (H / J, req/s) | p99 at max (H / J, ms) | throughput ratio |
|---|---|---|---|---|---|
{table}

> ratio = Ferrofin max RPS ÷ Jellyfin max RPS (>1 = Ferrofin sustains more). A `·`
> knee means p99 never crossed the threshold within the sustained ladder. The
> sweep ladder is coarse (×2 steps), so treat these as order-of-magnitude
> capacity, not exact ceilings.
"""
    Path(f"results/phaseB-{version}.md").write_text(md)


def main():
    """Dispatch: `a <version> <rate> <dur>` or `b <version>`."""
    args = sys.argv[1:]
    if args and args[0] == "a":
        version = args[1] if len(args) > 1 else "dev"
        rate = args[2] if len(args) > 2 else "?"
        dur = args[3] if len(args) > 3 else "?"
        render_a(version, rate, dur)
    elif args and args[0] == "b":
        render_b(args[1] if len(args) > 1 else "dev")
    else:
        raise SystemExit("usage: render_phases.py a <version> <rate> <dur> | b <version>")


if __name__ == "__main__":
    main()
