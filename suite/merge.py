#!/usr/bin/env python3
"""suite/merge.py — join parity (ledger) + perf (bench) into ONE run record (Plan 6, M2/M4).

Reads, all at a single Ferrofin SHA:
  - suite/registry.json          variant id → contract operation (the join key, M1)
  - suite/parity/ledger.json           per-op parity depth + deep_verified + classification
  - perf (per variant latencies), from EITHER
        suite/perf/results/raw/{ferrofin,jellyfin}-summary.json   (a fresh `suite/run.sh perf`)
        or the latest entry of suite/perf/bench-data.json       (fallback)
  - suite/results/raw/perf-fingerprints-{ferrofin,jellyfin}.json (optional, mid-run honesty check)

Writes suite/results/run-<sha>.json and upserts it into suite/results/runs.json (the trend the
viewer reads). Fairness rules baked in (not left to the reader):
  - a row is `comparable` only if the op is deep_verified, both servers answered 200 for it, and
    Ferrofin's body SHAPE matched Jellyfin's when both were captured during the perf leg (same
    library state — comparing across the parity/perf legs false-flags play-state fields);
  - the headline median-speedup / win-rate are computed over comparable rows ONLY;
  - a Ferrofin "win" requires beating Jellyfin on p50 AND p95 AND p99 — a p50 win with a tail loss
    is surfaced as a tail loss, never folded into "faster".
"""
import hashlib
import json
import os
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path
from statistics import median

ROOT = Path(__file__).resolve().parent.parent
SUITE = ROOT / "suite"
RESULTS = SUITE / "results"
RAW = RESULTS / "raw"


def run_signature(record):
    """Fingerprint a run's *measured* numbers so an exact re-merge of the same raw
    artifacts collapses to one entry, while a genuine rerun (which differs in every
    latency) stays distinct. Excludes meta.when, which changes on every merge."""
    perf = sorted(
        (o["perf"].get("variant"), o["perf"].get("h_p50"), o["perf"].get("j_p50"),
         o["perf"].get("h_p99"), o["perf"].get("j_p99"))
        for o in record.get("operations", [])
    )
    payload = json.dumps(
        {"headline": record.get("headline"),
         "footprint": record.get("meta", {}).get("footprint"),
         "perf": perf},
        sort_keys=True, default=str,
    )
    return hashlib.sha256(payload.encode()).hexdigest()


def bench_env(key):
    """os.environ, else suite/perf/.env — merge often runs without the bench env
    sourced, and the comparability guard keys on cpus/mem/load, so a silent
    None here would let incomparable runs look comparable."""
    if os.environ.get(key):
        return os.environ[key]
    try:
        for line in (SUITE / "perf" / ".env").read_text().splitlines():
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
    led = load_json(SUITE / "parity" / "ledger.json", {"operations": []})
    return {r["operation"]: r for r in led["operations"]}


def footprint():
    """Cold-start / peak-RSS / item-count per server from run.sh's raw files,
    when the perf leg was a full `run.sh` (the phase scripts don't write them).
    All-None when absent — the viewer just omits the footprint line."""
    raw = SUITE / "perf" / "results" / "raw"

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
    for tgt, key in (("ferrofin", "h"), ("jellyfin", "j")):
        out[f"{key}_cold_s"] = first(f"{tgt}-cold.txt")
        out[f"{key}_rss_peak_mib"] = peak(f"{tgt}-rss.txt")
        out[f"{key}_items"] = first(f"{tgt}-count.txt")
        # HLS play-start TTFS (ttfs.py, RUN_TRANSCODE=1): median time to
        # first segment for a stream-copy remux and a forced 4K HEVC->H.264
        # encode — the closest thing to "how fast does play start" and a
        # headline metric, never to be dropped from the record.
        tj = load_json(raw / f"{tgt}-transcode.json")
        for mode in ("copy", "encode"):
            m = (tj or {}).get(mode)
            out[f"{key}_ttfs_{mode}"] = (
                {"med": m["med"], "min": m["min"], "max": m["max"], "runs": m["runs"]}
                if m else None
            )
    return out if any(v is not None for v in out.values()) else None


def perf_by_variant():
    """{variant: {h_p50,h_p95,h_p99,h_rps,h_ok, j_*}} from raw summaries, else bench-data latest."""
    h = load_json(SUITE / "perf/results/raw/ferrofin-summary.json")
    j = load_json(SUITE / "perf/results/raw/jellyfin-summary.json")
    if h and j:
        out = {}
        for name, hv in h.get("endpoints", {}).items():
            jv = j.get("endpoints", {}).get(name, {})
            out[name] = {
                "h_p50": hv.get("p50"), "h_p95": hv.get("p95"), "h_p99": hv.get("p99"),
                "h_rps": hv.get("rps"), "h_ok": hv.get("okPct"),
                "j_p50": jv.get("p50"), "j_p95": jv.get("p95"), "j_p99": jv.get("p99"),
                "j_rps": jv.get("rps"), "j_ok": jv.get("okPct"),
                # Open-loop bookkeeping (G1): both sides must have been driven at
                # the SAME recorded arrival rate, and each must have held it.
                "rate": hv.get("target_rate"), "rate_source": hv.get("rate_source"),
                "h_rate_held": hv.get("rate_held"), "j_rate_held": jv.get("rate_held"),
                "j_rate": jv.get("target_rate"),
            }
        return out, "raw-summary"
    bd = load_json(SUITE / "perf" / "bench-data.json")
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
    media = SUITE / "perf" / "fixtures" / "media"
    if media.is_dir():
        for f in sorted(media.rglob("*")):
            if f.is_file():
                h.update(f"{f.relative_to(media)} {f.stat().st_size}\n".encode())
    return h.hexdigest()[:16]


def server_build():
    """The /health/live build identity run.sh captured for the ferrofin leg."""
    p = SUITE / "perf" / "results" / "raw" / "ferrofin-build.txt"
    try:
        return p.read_text().strip() or None
    except OSError:
        return None


def manifest_check(v2op, perf, foot):
    """A1 (fail loud): every registry bench variant must have produced a latency
    row on BOTH servers, and each declared special leg (TTFS copy/encode, when
    RUN_TRANSCODE=1) its footprint block. A leg that silently vanished — the
    2026-08 transcode.js path break produced two green runs with zero TTFS
    rows — must fail the merge, never thin the record.

    Returns (skipped, missing): SKIP_VARIANTS (comma list) records a variant as
    deliberately skipped instead of missing; anything else absent is missing.
    """
    skip = {s.strip() for s in os.environ.get("SKIP_VARIANTS", "").split(",") if s.strip()}
    missing = []
    for variant in sorted(v2op):
        if variant in skip:
            continue
        p = perf.get(variant)
        h_absent = p is None or p.get("h_p50") is None
        j_absent = p is None or p.get("j_p50") is None
        if h_absent or j_absent:
            side = "both" if h_absent and j_absent else "ferrofin" if h_absent else "jellyfin"
            missing.append(f"{variant}[{side}]")
    if bench_env("RUN_TRANSCODE") == "1":
        for key, tgt in (("h", "ferrofin"), ("j", "jellyfin")):
            for mode in ("copy", "encode"):
                leg = f"ttfs_{mode}"
                if leg in skip:
                    continue
                if not (foot or {}).get(f"{key}_{leg}"):
                    missing.append(f"{leg}[{tgt}]")
    return sorted(skip), sorted(missing)


def wins_all_three(p):
    v = [p["h_p50"], p["h_p95"], p["h_p99"], p["j_p50"], p["j_p95"], p["j_p99"]]
    if any(x is None for x in v):
        return False
    return p["h_p50"] < p["j_p50"] and p["h_p95"] < p["j_p95"] and p["h_p99"] < p["j_p99"]


def main():
    v2op = variant_to_op()
    par = parity_by_op()
    perf, perf_src = perf_by_variant()
    fp_h = load_json(RAW / "perf-fingerprints-ferrofin.json", {})
    fp_j = load_json(RAW / "perf-fingerprints-jellyfin.json", {})
    foot = footprint()
    perf_meta = (load_json(SUITE / "perf/results/raw/ferrofin-summary.json") or {}).get("meta")

    # A1: measure the full manifest or fail loud (no green record with holes).
    # MERGE_ALLOW_INCOMPLETE=1 downgrades to a record stamped `incomplete` that
    # is written but kept OUT of the trend file (needed for the legacy
    # bench-data fallback, which predates the full endpoint set).
    skipped, missing = manifest_check(v2op, perf, foot)
    if missing:
        print(f"!! manifest incomplete — {len(missing)} expected leg(s) produced no data:", file=sys.stderr)
        for m in missing:
            print(f"!!   {m}", file=sys.stderr)
        print("!! (deliberate? record it: SKIP_VARIANTS=name1,name2 — skipped legs are stamped into the record)", file=sys.stderr)
        if os.environ.get("MERGE_ALLOW_INCOMPLETE") != "1":
            print("!! refusing to write a run record (MERGE_ALLOW_INCOMPLETE=1 to write one stamped incomplete, excluded from the trend)", file=sys.stderr)
            sys.exit(2)

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

        # Write (non-GET) rows are fingerprint-exempt: fingerprint.py never captures
        # them (a probe would mutate state), so their honesty gate is deep_verified —
        # the parity WRITE JOURNEY — plus the 100% expected-status check below.
        is_write = not op.startswith("GET ")
        drift = (not is_write) and op in fp_h and op in fp_j and fp_h[op] != fp_j[op]
        both_ok = p.get("h_ok") == 100 and p.get("j_ok") == 100
        have_lat = p["h_p50"] is not None and p["j_p50"] is not None
        # G1: a latency comparison is only meaningful at the same arrival rate,
        # actually held on both sides. Legacy (closed-loop) records carry no
        # rate keys — None == None keeps them flowing through unchanged.
        same_rate = p.get("rate") == p.get("j_rate")
        rates_held = p.get("h_rate_held") is not False and p.get("j_rate_held") is not False
        comparable = deep and both_ok and have_lat and not drift and same_rate and rates_held
        reason = (None if comparable else
                  "body shape diverges from Jellyfin at bench time" if drift else
                  "not deep-verified" if not deep else
                  "200-rate < 100%" if not both_ok else
                  "measured at different arrival rates" if not same_rate else
                  "open-loop rate not held" if not rates_held else "missing latency")

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
    # A2: surface how many rows fell out of the comparison and why, so shrinking
    # coverage reads as shrinking coverage instead of "all green".
    dropped = {}
    for o in operations:
        r = o["perf"]["reason"]
        if r:
            dropped[r] = dropped.get(r, 0) + 1
    headline = {
        "comparable_rows": len(comp),
        "dropped_rows": sum(dropped.values()),
        "dropped_by_reason": dropped,
        "median_speedup": round(median(speedups), 3) if speedups else None,
        "win_rate": round(sum(o["win_all_three"] for o in comp) / len(comp), 3) if comp else None,
        "tail_losses": [o["variant"] for o in comp if o["tail_loss"]],
        "parity_coverage": round(len(deep_ops) / len(benched_ops), 3) if benched_ops else None,
    }

    sha = sh("git", "rev-parse", "--short", "HEAD") or "unknown"
    record = {
        "meta": {
            "ferrofin": sh("git", "describe", "--tags", "--always") or "dev",
            "ferrofin_sha": sha,
            "jellyfin_image": bench_env("JELLYFIN_IMAGE") or "jellyfin/jellyfin:10.11.8",
            "fixture_hash": fixture_hash(),
            "cpus": int(bench_env("BENCH_CPUS")) if bench_env("BENCH_CPUS") else None,
            "mem": bench_env("BENCH_MEM"),
            # The load model + engine + resolved methodology knobs come from the
            # perf leg's own summary meta (compare.py) — self-describing records.
            # Old (closed-loop k6) records carry {"vus","duration"} here instead;
            # the comparability guard keys on this, so the two never compare.
            "load": {"model": "open-loop",
                     "duration_secs": (perf_meta or {}).get("bench_config", {})
                     .get("values", {}).get("BENCH_DURATION_SECS")},
            "engine": (perf_meta or {}).get("engine"),
            "generator_ceiling_rps": (perf_meta or {}).get("generator_ceiling_rps"),
            "bench_config": (perf_meta or {}).get("bench_config"),
            "perf_source": perf_src,
            "footprint": foot,
            # B1: the build identity the running server reported over
            # /health/live during the perf leg (run.sh verified it against the
            # tree before measuring); None for records that predate the check.
            "server_build": server_build(),
            "skipped_variants": skipped or None,
            "incomplete": missing or None,
            "when": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%MZ"),
        },
        "headline": headline,
        "operations": sorted(operations, key=lambda o: o["perf"]["variant"]),
    }

    RESULTS.mkdir(parents=True, exist_ok=True)

    # An incomplete record (MERGE_ALLOW_INCOMPLETE=1) is written for inspection
    # under its own name but NEVER enters the trend file — a record with holes
    # must not look like a point on the same curve as complete ones.
    if missing:
        out = RESULTS / f"run-{sha}-incomplete.json"
        record["meta"]["run_label"] = f"{record['meta']['ferrofin']} (incomplete)"
        out.write_text(json.dumps(record, indent=2) + "\n")
        print(f">> wrote {out.relative_to(ROOT)} — INCOMPLETE, excluded from the trend")
        return

    # Keep every distinct run of the same SHA (variance across reruns is the point) — but
    # collapse an exact re-merge of the same raw artifacts so a second `run.sh merge`
    # doesn't double-count one measurement. Genuine reruns differ in every latency, so
    # their signatures differ; a re-merge is identical and overwrites its own entry.
    runs = load_json(RESULTS / "runs.json", {"runs": []})
    sig = run_signature(record)
    same_sha = [r for r in runs["runs"] if r["meta"]["ferrofin_sha"] == sha]
    dup = next((r for r in same_sha if run_signature(r) == sig), None)
    seq = dup["meta"].get("run_seq", 1) if dup else len(same_sha) + 1
    record["meta"]["run_seq"] = seq
    record["meta"]["run_label"] = record["meta"]["ferrofin"] if seq == 1 \
        else f"{record['meta']['ferrofin']} ({seq})"

    # Numbered filename so reruns of one SHA don't clobber each other's record file.
    out = RESULTS / (f"run-{sha}.json" if seq == 1 else f"run-{sha}-{seq}.json")
    out.write_text(json.dumps(record, indent=2) + "\n")

    # Upsert into the trend file the viewer reads (newest last): replace only an exact
    # re-merge (same sha + identical numbers); otherwise append, preserving prior reruns.
    if dup:
        runs["runs"] = [record if r is dup else r for r in runs["runs"]]
    else:
        runs["runs"] = runs["runs"] + [record]
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
