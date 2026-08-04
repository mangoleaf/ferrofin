#!/usr/bin/env python3
"""suite/merge.py — join parity (ledger) + perf (bench) into ONE run record (Plan 6, M2/M4).

Reads, all at a single Hermit SHA:
  - suite/registry.json          variant id → contract operation (the join key, M1)
  - parity/ledger.json           per-op parity depth + deep_verified + classification
  - perf (per variant latencies), from EITHER
        benchmark/results/raw/{hermit,jellyfin}-summary.json   (a fresh `suite/run.sh perf`)
        or the latest entry of benchmark/bench-data.json       (fallback)
  - suite/results/raw/{parity,perf}-fingerprints.json          (optional, mid-run honesty check)

Writes suite/results/run-<sha>.json and upserts it into suite/results/runs.json (the trend the
viewer reads). Fairness rules baked in (not left to the reader):
  - a row is `comparable` only if the op is deep_verified, both servers answered 200 for it, and
    its body fingerprint didn't drift since the parity pass;
  - the headline median-speedup / win-rate are computed over comparable rows ONLY;
  - a Hermit "win" requires beating Jellyfin on p50 AND p95 AND p99 — a p50 win with a tail loss
    is surfaced as a tail loss, never folded into "faster".
"""
import hashlib
import json
import os
import subprocess
from datetime import datetime, timezone
from pathlib import Path
from statistics import median

ROOT = Path(__file__).resolve().parent.parent
SUITE = ROOT / "suite"
RESULTS = SUITE / "results"
RAW = RESULTS / "raw"


def bench_env(key):
    """os.environ, else benchmark/.env — merge often runs without the bench env
    sourced, and the comparability guard keys on cpus/mem/load, so a silent
    None here would let incomparable runs look comparable."""
    if os.environ.get(key):
        return os.environ[key]
    try:
        for line in (ROOT / "benchmark" / ".env").read_text().splitlines():
            line = line.strip()
            if line.startswith(f"{key}="):
                return line.split("=", 1)[1].strip() or None
    except OSError:
        pass
    return None


def sh(*cmd):
    try:
        return subprocess.check_output(cmd, cwd=ROOT, text=True, stderr=subprocess.DEVNULL).strip()
    except Exception:
        return ""


def load_json(p, default=None):
    return json.loads(Path(p).read_text()) if Path(p).exists() else default


def variant_to_op():
    reg = load_json(SUITE / "registry.json")["operations"]
    return {v["id"]: (e["op"], e["tag"]) for e in reg for v in e["variants"]}


def parity_by_op():
    led = load_json(ROOT / "parity" / "ledger.json", {"operations": []})
    return {r["operation"]: r for r in led["operations"]}


def footprint():
    """Cold-start / peak-RSS / item-count per server from run.sh's raw files,
    when the perf leg was a full `run.sh` (the phase scripts don't write them).
    All-None when absent — the viewer just omits the footprint line."""
    raw = ROOT / "benchmark" / "results" / "raw"

    def read1(name):
        p = raw / name
        try:
            return p.read_text().strip().splitlines()
        except OSError:
            return []

    def first(name):
        lines = read1(name)
        return lines[0].strip() if lines else None

    def peak(name):
        vals = []
        for line in read1(name):
            try:
                vals.append(float(line))
            except ValueError:
                pass
        return round(max(vals)) if vals else None

    out = {}
    for tgt, key in (("hermit", "h"), ("jellyfin", "j")):
        out[f"{key}_cold_s"] = first(f"{tgt}-cold.txt")
        out[f"{key}_rss_peak_mib"] = peak(f"{tgt}-rss.txt")
        out[f"{key}_items"] = first(f"{tgt}-count.txt")
    return out if any(v is not None for v in out.values()) else None


def perf_by_variant():
    """{variant: {h_p50,h_p95,h_p99,h_rps,h_ok, j_*}} from raw summaries, else bench-data latest."""
    h = load_json(RAW.parent.parent.parent / "benchmark/results/raw/hermit-summary.json")
    j = load_json(RAW.parent.parent.parent / "benchmark/results/raw/jellyfin-summary.json")
    if h and j:
        out = {}
        for name, hv in h.get("endpoints", {}).items():
            jv = j.get("endpoints", {}).get(name, {})
            out[name] = {
                "h_p50": hv.get("p50"), "h_p95": hv.get("p95"), "h_p99": hv.get("p99"),
                "h_rps": hv.get("rps"), "h_ok": hv.get("okPct"),
                "j_p50": jv.get("p50"), "j_p95": jv.get("p95"), "j_p99": jv.get("p99"),
                "j_rps": jv.get("rps"), "j_ok": jv.get("okPct"),
            }
        return out, "raw-summary"
    bd = load_json(ROOT / "benchmark" / "bench-data.json")
    if bd and bd.get("versions"):
        latest = bd["versions"][-1]
        out = {e["name"]: {
            "h_p50": e.get("h_p50"), "h_p95": e.get("h_p95"), "h_p99": e.get("h_p99"),
            "h_rps": e.get("h_rps"), "h_ok": e.get("h_ok"),
            "j_p50": e.get("j_p50"), "j_p95": e.get("j_p95"), "j_p99": e.get("j_p99"),
            "j_rps": e.get("j_rps"), "j_ok": e.get("j_ok"),
        } for e in latest.get("endpoints", [])}
        return out, "bench-data-latest"
    return {}, "none"


def fixture_hash():
    """sha256 over the synthetic fixture manifest + the libraries env — the comparability key.
    ponytail: for real-media-only runs this hashes the LIBRARIES config, not the host files
    (unreachable from here). Upgrade: hash a manifest emitted by gen-fixtures.sh for real dirs too."""
    h = hashlib.sha256()
    h.update((os.environ.get("LIBRARIES", "") + "\n").encode())
    media = ROOT / "benchmark" / "fixtures" / "media"
    if media.is_dir():
        for f in sorted(media.rglob("*")):
            if f.is_file():
                h.update(f"{f.relative_to(media)} {f.stat().st_size}\n".encode())
    return h.hexdigest()[:16]


def wins_all_three(p):
    v = [p["h_p50"], p["h_p95"], p["h_p99"], p["j_p50"], p["j_p95"], p["j_p99"]]
    if any(x is None for x in v):
        return False
    return p["h_p50"] < p["j_p50"] and p["h_p95"] < p["j_p95"] and p["h_p99"] < p["j_p99"]


def main():
    v2op = variant_to_op()
    par = parity_by_op()
    perf, perf_src = perf_by_variant()
    fp_par = load_json(RAW / "parity-fingerprints.json", {})
    fp_perf = load_json(RAW / "perf-fingerprints.json", {})

    operations, benched_ops, deep_ops = [], set(), set()
    for variant, p in perf.items():
        op, tag = v2op.get(variant, (None, None))
        if op is None:
            continue  # a benched name not in the registry — self-test would have caught new ones
        pr = par.get(op, {})
        deep = bool(pr.get("deep_verified"))
        benched_ops.add(op)
        if deep:
            deep_ops.add(op)

        drift = op in fp_par and op in fp_perf and fp_par[op] != fp_perf[op]
        both_ok = p.get("h_ok") == 100 and p.get("j_ok") == 100
        have_lat = p["h_p50"] is not None and p["j_p50"] is not None
        comparable = deep and both_ok and have_lat and not drift
        reason = (None if comparable else
                  "body drifted since parity pass" if drift else
                  "not deep-verified" if not deep else
                  "200-rate < 100%" if not both_ok else "missing latency")

        win = wins_all_three(p)
        speedup = round(p["j_p50"] / p["h_p50"], 2) if have_lat and p["h_p50"] else None
        operations.append({
            "op": op, "tag": tag,
            "parity": {"depth": pr.get("depth"), "deep_verified": deep,
                       "classification": pr.get("classification") or None},
            "perf": {"variant": variant, **p, "speedup": speedup,
                     "win_all_three": win,
                     "tail_loss": bool(comparable and speedup and speedup > 1 and not win),
                     "comparable": comparable, "reason": reason},
        })

    comp = [o["perf"] for o in operations if o["perf"]["comparable"]]
    speedups = [o["speedup"] for o in comp if o["speedup"] is not None]
    headline = {
        "comparable_rows": len(comp),
        "median_speedup": round(median(speedups), 3) if speedups else None,
        "win_rate": round(sum(o["win_all_three"] for o in comp) / len(comp), 3) if comp else None,
        "tail_losses": [o["variant"] for o in comp if o["tail_loss"]],
        "parity_coverage": round(len(deep_ops) / len(benched_ops), 3) if benched_ops else None,
    }

    sha = sh("git", "rev-parse", "--short", "HEAD") or "unknown"
    record = {
        "meta": {
            "hermit": sh("git", "describe", "--tags", "--always") or "dev",
            "hermit_sha": sha,
            "jellyfin_image": bench_env("JELLYFIN_IMAGE") or "jellyfin/jellyfin:10.11.8",
            "fixture_hash": fixture_hash(),
            "cpus": int(bench_env("BENCH_CPUS")) if bench_env("BENCH_CPUS") else None,
            "mem": bench_env("BENCH_MEM"),
            "load": {"vus": int(bench_env("BENCH_VUS")) if bench_env("BENCH_VUS") else None,
                     "duration": bench_env("BENCH_DURATION")},
            "perf_source": perf_src,
            "footprint": footprint(),
            "when": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%MZ"),
        },
        "headline": headline,
        "operations": sorted(operations, key=lambda o: o["perf"]["variant"]),
    }

    RESULTS.mkdir(parents=True, exist_ok=True)
    out = RESULTS / f"run-{sha}.json"
    out.write_text(json.dumps(record, indent=2) + "\n")

    # Upsert into the trend file the viewer reads (keyed by sha; newest last).
    runs = load_json(RESULTS / "runs.json", {"runs": []})
    runs["runs"] = [r for r in runs["runs"] if r["meta"]["hermit_sha"] != sha] + [record]
    (RESULTS / "runs.json").write_text(json.dumps(runs, indent=2) + "\n")

    hl = headline
    print(f">> wrote {out.relative_to(ROOT)}  (perf: {perf_src})")
    print(f"   comparable rows: {hl['comparable_rows']}/{len(operations)}  "
          f"median speedup: {hl['median_speedup']}  win-rate: {hl['win_rate']}  "
          f"parity coverage: {hl['parity_coverage']}")
    if hl["tail_losses"]:
        print(f"   tail losses (p50 win, p95/p99 loss): {', '.join(hl['tail_losses'])}")


if __name__ == "__main__":
    main()
