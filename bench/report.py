#!/usr/bin/env python3
"""Render a run (or several) as the README tables (PLAN_BENCHMARK_V3 §7). stdlib only.

    report.py RUN_DIR [RUN_DIR ...]

One run dir: the numbers of that run. Several: each cell is `median (min–max)` across
runs, and a cell whose spread exceeds SPREAD_MAX of its median prints "not reproducible".
Two rules are applied per row, both from files the run itself wrote:
  comparable   — same status + record count as the oracle (Jellyfin 10.11.8) and a field
                 set ⊇ the oracle's, for every request name in the row (shape.log);
  reproducible — spread within SPREAD_MAX across runs.
Missing phases are named, never silently dropped.
"""

import csv
import json
import os
import statistics
import sys
from collections import defaultdict

SPREAD_MAX = 0.15
ORACLE = "jellyfin"
SERVERS = [("jellyfin", "Jellyfin 10.11.8"), ("jellyfin12", "Jellyfin 12.0-rc7"), ("ferrofin", "Ferrofin")]
SCREENS = ["home", "movies", "detail", "series", "search", "playback"]
MIB = 2 ** 20


def load(path):
    try:
        return json.load(open(path))
    except (OSError, ValueError):
        return None


def load_shape(d):
    """shape.log → {request name: {status set, counts, fields}} merged over the responses."""
    out = defaultdict(lambda: {"status": set(), "count": set(), "fields": set(), "n": 0})
    try:
        lines = open(os.path.join(d, "shape.log")).read().splitlines()
    except OSError:
        return None
    for line in lines:
        try:
            rec = json.loads(json.loads(line)["msg"])
        except (ValueError, KeyError, TypeError):
            continue
        s = out[rec["shape"]]
        s["status"].add(rec.get("status"))
        s["n"] += 1
        if "count" in rec and rec["count"] is not None:
            s["count"].add(rec["count"])
        s["fields"].update(rec.get("fields") or [])
    return out


def oracle_failed(shape_oracle, names):
    """The oracle's own verdict: a status >= 400 on any request behind the row."""
    if shape_oracle is None:
        return "no shape pass"  # the oracle ran but its shape phase did not
    for n in names:
        o = shape_oracle.get(n)
        if o and any(st >= 400 for st in o["status"]):
            return f"Jellyfin failed {n} ({sorted(o['status'])})"
    return None


def comparable(shape_srv, shape_oracle, names):
    """None if comparable, else the reason."""
    if shape_oracle is None:
        return "no oracle run (Jellyfin 10.11.8 not in this run)"
    if shape_srv is None:
        return "no shape pass"
    for n in names:
        o, s = shape_oracle.get(n), shape_srv.get(n)
        if o is None or s is None:
            return f"{n}: missing"
        if any(st >= 400 for st in o["status"]):
            return f"{n}: Jellyfin failed ({sorted(o['status'])})"
        if s["status"] != o["status"]:
            return f"{n}: status {sorted(s['status'])} vs {sorted(o['status'])}"
        if s["count"] != o["count"]:
            return f"{n}: count {sorted(s['count'])} vs {sorted(o['count'])}"
        missing = sorted(o["fields"] - s["fields"])
        if missing:
            more = f" (+{len(missing) - 3} more)" if len(missing) > 3 else ""
            return f"{n}: missing {', '.join(m.lstrip('.') for m in missing[:3])}{more}"
    return None


def mem_numbers(d):
    win = load(os.path.join(d, "windows.json"))
    try:
        rows = list(csv.DictReader(open(os.path.join(d, "mem.csv"))))
    except OSError:
        return None
    if not win or not rows:
        return None

    def within(w):
        return [int(r["anon"]) for r in rows if w["start"] <= float(r["t"]) <= w["end"]]

    out = {}
    if "loaded" in win and within(win["loaded"]):
        out["peak"] = max(within(win["loaded"])) / MIB
    if "steady" in win and within(win["steady"]):
        out["steady"] = statistics.median(within(win["steady"])) / MIB
    if "interference" in rows[0]:
        out["interference"] = max(float(r["interference"]) for r in rows)
    if "swap" in rows[0]:
        out["swap_max"] = max(int(r["swap"]) for r in rows) / MIB
    return out


def agg(values, fmt):
    """values across runs (None = missing) → cell text with the reproducibility rule."""
    vs = [v for v in values if v is not None]
    if not vs:
        return "—"
    med = statistics.median(vs)
    if len(vs) == 1:
        return fmt(med)
    spread = (max(vs) - min(vs)) / med if med else 0
    cell = f"{fmt(med)} ({fmt(min(vs))}–{fmt(max(vs))})"
    return cell if spread <= SPREAD_MAX else f"not reproducible: {cell}"


ms = lambda v: f"{v:.0f} ms"
mib = lambda v: f"{v:.0f} MiB"


def main():
    runs = sys.argv[1:]
    if not runs:
        sys.exit(__doc__)
    meta = load(os.path.join(runs[0], "run.json")) or {}
    servers = [(k, label) for k, label in SERVERS if any(os.path.isdir(os.path.join(r, k)) for r in runs)]
    per = {k: [os.path.join(r, k) for r in runs] for k, _ in servers}
    shapes = {k: [load_shape(d) for d in ds] for k, ds in per.items()}
    missing = []

    print(f"## Ferrofin vs Jellyfin — {len(runs)} run(s), commit {meta.get('sha', '?')}, {meta.get('date', '')[:10]}")
    print(f"Host {meta.get('cpu', '?')} · server on cpus {meta.get('server_cpus', '?')} · {meta.get('memory_limit', '?')} limit · "
          f"test data {meta.get('testdata_counts', {})} · windows {meta.get('window_s', '?')} s · "
          f"unloaded {meta.get('rate_unloaded', '?')} screens/s · loaded {meta.get('rate_loaded', '?')} screens/s")
    print("Cells: median (min–max) across runs. `⚠ not comparable` = the server did different work than Jellyfin 10.11.8 "
          "(status / record count / missing fields) — its raw number is shown for the work list but is not publishable; "
          "`not reproducible` = spread > 15 % of the median.\n")

    # ── latency: screens then endpoints, per level ──────────────────────────
    for level in ("unloaded", "loaded"):
        data = {k: [load(os.path.join(d, f"k6-{level}.json")) for d in ds] for k, ds in per.items()}
        for k, ds in per.items():
            if any(x is None for x in data[k]):
                missing.append(f"{k}: k6-{level}.json")
        names_of = defaultdict(set)
        for k in data:
            for x in data[k]:
                if x:
                    for n in x["endpoints"]:
                        names_of[n.split(":")[0]].add(n)
        print(f"### Latency — {level} ({meta.get('rate_' + level, '?')} screens/s)\n")
        head = "| screen | " + " | ".join(f"{label} p50 / p95 / p99 (err)" for _, label in servers) + " |"
        print(head + "\n|" + "---|" * (len(servers) + 1))
        dropped = {k: sum((x or {}).get("dropped_iterations", 0) for x in data[k]) for k in data}
        oracle_shape = shapes[ORACLE][0] if ORACLE in shapes else None

        def flag(k, names):
            if dropped.get(k):
                return f"invalid window: k6 dropped {dropped[k]} iterations"
            if k == ORACLE:
                return oracle_failed(oracle_shape, names)
            return comparable(shapes[k][0], oracle_shape, names)

        # a screen row is judged on its API requests; poster fetches ('image') show up in the
        # image endpoint row and in err%, and the list endpoints' ImageTags superset check
        # already proves the same posters exist on every server
        for scr in SCREENS:
            cells = []
            for k, _ in servers:
                reason = flag(k, sorted(names_of[scr]))
                vals = [x["screens"].get(scr) if x else None for x in data[k]]
                cells.append(f"⚠ not comparable ({reason}) · raw {cell_of(vals)}" if reason else cell_of(vals))
            print(f"| {scr} | " + " | ".join(cells) + " |")
        print()
        print("| endpoint | " + " | ".join(f"{label} p50 / p95 / p99 (err)" for _, label in servers) + " |")
        print("|" + "---|" * (len(servers) + 1))
        for n in [n for scr in SCREENS for n in sorted(names_of[scr])] + (["image"] if "image" in names_of else []):
            cells = []
            for k, _ in servers:
                reason = flag(k, [n])
                raw = cell_of([x["endpoints"].get(n) if x else None for x in data[k]])
                cells.append(f"⚠ not comparable ({reason}) · raw {raw}" if reason else raw)
            print(f"| {n} | " + " | ".join(cells) + " |")
        print()

    # ── time to first screen ────────────────────────────────────────────────
    print("### Time to first screen\n")
    print("| | " + " | ".join(label for _, label in servers) + " |\n|" + "---|" * (len(servers) + 1))
    cold, hls, direct, turl = [], [], [], {}
    for k, ds in per.items():
        cs = [load(os.path.join(d, "coldstart.json")) for d in ds]
        if any(c is None for c in cs):
            missing.append(f"{k}: coldstart.json")
        cold.append(agg([statistics.median([r["home_ms"] for r in c["runs"] if r["home_ms"]]) if c and any(r["home_ms"] for r in c["runs"]) else None for c in cs], ms))
        ts = [load(os.path.join(d, "ttfs.json")) for d in ds]
        if any(t is None for t in ts):
            missing.append(f"{k}: ttfs.json")
        hls.append(agg([statistics.median([h["ttfs_ms"] for h in t["hls"] if "ttfs_ms" in h]) if t and any("ttfs_ms" in h for h in t["hls"]) else None for t in ts], ms))
        direct.append(agg([statistics.median([h["ttfb_ms"] for h in t["direct"] if "ttfb_ms" in h]) if t and any("ttfb_ms" in h for h in t["direct"]) else None for t in ts], ms))
        for t in ts:
            for h in (t or {}).get("hls", []):
                if h.get("transcoding_url"):
                    # ffmpeg-relevant parameters only: session/auth/tag identify the request and
                    # TranscodeReasons is informational (it differs between versions without changing a single ffmpeg argument)
                    turl[k] = sorted(p.lower() for p in h["transcoding_url"].split("?")[-1].split("&")
                                     if p and not p.lower().startswith(("playsessionid", "apikey", "deviceid", "tag=", "api_key", "transcodereasons")))
                    break
        errs = [h["error"] for t in ts if t for h in t["hls"] if "error" in h]
        if errs:
            hls[-1] += f" ({len(errs)} failed: {errs[0][:60]})"
    if ORACLE in turl:
        for i, (k, _) in enumerate(servers):
            if k != ORACLE and k in turl and turl[k] != turl[ORACLE]:
                diff = sorted(set(turl[k]) ^ set(turl[ORACLE]))
                hls[i] = f"⚠ not comparable (transcode parameters differ: {', '.join(diff)[:200]}) · raw {hls[i]}"
    print("| cold start (restart → home screen) | " + " | ".join(cold) + " |")
    print("| HLS first segment (forced transcode) | " + " | ".join(hls) + " |")
    print("| direct-play TTFB (1 MiB range) | " + " | ".join(direct) + " |")
    print()

    # ── memory ──────────────────────────────────────────────────────────────
    sample_ms = meta.get("mem_sample_ms", "?")
    print(f"### Memory (anon, cache excluded, {sample_ms} ms samples)\n")
    print("| | " + " | ".join(label for _, label in servers) + " |\n|" + "---|" * (len(servers) + 1))
    mems = {k: [mem_numbers(d) for d in ds] for k, ds in per.items()}
    for k, m in mems.items():
        if any(x is None for x in m):
            missing.append(f"{k}: mem.csv/windows.json")
    # the peak is only meaningful if the loaded window that produced it was the specified load
    loaded = {k: [load(os.path.join(d, "k6-loaded.json")) for d in ds] for k, ds in per.items()}
    peak = []
    for k, _ in servers:
        cell = agg([(m or {}).get("peak") for m in mems[k]], mib)
        bad = [x for x in loaded[k] if x is None or x.get("dropped_iterations")]
        peak.append(f"⚠ not comparable (loaded window missing or dropped iterations) · raw {cell}" if bad else cell)
    print("| peak under load | " + " | ".join(peak) + " |")
    print("| steady idle | " + " | ".join(agg([(m or {}).get("steady") for m in mems[k]], mib) for k, _ in servers) + " |")
    print(f"| max interference on the server's cores (share of a {sample_ms} ms sample not spent by the container) | " + " | ".join(agg([(m or {}).get("interference") for m in mems[k]], lambda v: f"{v * 100:.0f}%") for k, _ in servers) + " |")
    print("| max swap | " + " | ".join(agg([(m or {}).get("swap_max") for m in mems[k]], mib) for k, _ in servers) + " |")
    print()

    # ── shape buckets (supporting evidence for parity work) ────────────────
    if ORACLE in shapes and shapes[ORACLE][0]:
        print("### Response shape vs Jellyfin 10.11.8 (supporting evidence, not the parity number)\n")
        for k, _ in servers:
            if k == ORACLE or not shapes[k][0]:
                continue
            match, diverge, failed = [], [], []
            for n in sorted(shapes[ORACLE][0]):
                reason = comparable(shapes[k][0], shapes[ORACLE][0], [n])
                (failed if reason and "Jellyfin failed" in reason else diverge if reason else match).append((n, reason))
            print(f"**{k}**: {len(match)} match · {len(diverge)} diverge · {len(failed)} Jellyfin failed")
            for n, r in diverge:
                print(f"- diverges — {r}")
            for n, r in failed:
                print(f"- {r}")
            print()

    counts = {k: [load(os.path.join(d, "counts.json")) for d in ds] for k, ds in per.items()}
    if ORACLE in counts and counts[ORACLE][0]:
        for k, c in counts.items():
            if k != ORACLE and c[0] and c[0] != counts[ORACLE][0]:
                diff = {n: (c[0].get(n), counts[ORACLE][0].get(n)) for n in counts[ORACLE][0] if c[0].get(n) != counts[ORACLE][0].get(n)}
                print(f"**item counts differ** ({k} vs {ORACLE}): {diff}\n")

    if missing:
        print("### Missing phases\n")
        for m in sorted(set(missing)):
            print(f"- {m}")


def cell_of(vals):
    """per-run k6 summaries for one screen/endpoint → 'p50 / p95 / p99 (err%)' with spread rules."""
    if all(v is None for v in vals):
        return "—"
    p50 = agg([v["p50"] if v else None for v in vals], lambda v: f"{v:.0f}")
    p95 = agg([v["p95"] if v else None for v in vals], lambda v: f"{v:.0f}")
    p99 = agg([v["p99"] if v else None for v in vals], lambda v: f"{v:.0f}")
    err = max((1 - v["ok"]) * 100 for v in vals if v and v["ok"] is not None) if any(v and v["ok"] is not None for v in vals) else 0
    return f"{p50} / {p95} / {p99} ms ({err:.1f}%)"


if __name__ == "__main__":
    main()
