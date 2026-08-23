#!/usr/bin/env python3
"""suite/merge.py — join parity (ledger) + perf (bench) into ONE run record (Plan 6, M2/M4).

Reads, all at a single Ferrofin SHA:
  - suite/registry.json          variant id → contract operation (the join key, M1)
  - suite/parity/ledger.json           per-op parity depth + deep_verified + classification
  - perf (per variant latencies), from EITHER
        suite/perf/results/raw/{ferrofin,jellyfin}-summary.json   (a fresh `suite/run.sh perf`)
        or the latest entry of suite/perf/bench-data.json       (fallback)
  - suite/results/raw/perf-fingerprints-{ferrofin,jellyfin}.json (optional, mid-run honesty check)
  - suite/results/shape-baseline.json           the reviewed per-variant body-shape baseline

Writes suite/results/run-<sha>.json and upserts it into suite/results/runs.json (the trend the
viewer reads). Fairness rules baked in (not left to the reader):
  - a row is `comparable` only if the op is deep_verified, both servers answered 200 for it, and
    Ferrofin's body SHAPE matches the reviewed shape baseline — an UNREVIEWED change in
    Ferrofin's own body is exactly "fast because the body went hollow" and excludes the row
    until acked (MERGE_ACK_SHAPES=1 advances the baseline). Ferrofin-vs-Jellyfin shape is
    published on the row as information but NEVER excludes: the parity ledger owns that
    verdict per-op, and gating on it silently exiled every documented divergence forever;
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

# The methodology knobs (three-layer resolution: default < bench.conf < env)
# live in suite/perf/config.py — the manifest check needs the cold-endpoint
# list resolved the same way the measuring leg resolved it.
sys.path.insert(0, str(SUITE / "perf"))
from config import CONFIG  # noqa: E402


def run_signature(record):
    """Fingerprint a run's *measured* numbers so an exact re-merge of the same raw
    artifacts collapses to one entry, while a genuine rerun (which differs in every
    latency) stays distinct. Excludes meta.when (changes on every merge) and the
    headline: exclusion OUTCOMES (comparable set, shape acks) differ between a
    pre-ack merge and its MERGE_ACK_SHAPES re-merge of the same artifacts, and
    hashing them turned that documented workflow into a double-counted trend point
    (the re-merge must REPLACE the pre-ack record, not append beside it)."""
    perf = sorted(
        (o["perf"].get("variant"), o["perf"].get("f_p50"), o["perf"].get("j_p50"),
         o["perf"].get("f_p99"), o["perf"].get("j_p99"))
        for o in record.get("operations", [])
    )
    payload = json.dumps(
        {"footprint": record.get("meta", {}).get("footprint"),
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
    for tgt, key in (("ferrofin", "f"), ("jellyfin", "j")):
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
    """{variant: {f_p50,f_p95,f_p99,f_rps,f_ok, j_*}} (f=Ferrofin, j=Jellyfin) from raw summaries, else bench-data latest."""
    h = load_json(SUITE / "perf/results/raw/ferrofin-summary.json")
    j = load_json(SUITE / "perf/results/raw/jellyfin-summary.json")
    if h and j:
        out = {}
        for name, hv in h.get("endpoints", {}).items():
            jv = j.get("endpoints", {}).get(name, {})
            out[name] = {
                "f_p50": hv.get("p50"), "f_p95": hv.get("p95"), "f_p99": hv.get("p99"),
                "f_rps": hv.get("rps"), "f_ok": hv.get("okPct"),
                "j_p50": jv.get("p50"), "j_p95": jv.get("p95"), "j_p99": jv.get("p99"),
                "j_rps": jv.get("rps"), "j_ok": jv.get("okPct"),
                # Open-loop bookkeeping (G1): both sides must have been driven at
                # the SAME recorded arrival rate, and each must have held it.
                "rate": hv.get("target_rate"), "rate_source": hv.get("rate_source"),
                "f_rate_held": hv.get("rate_held"), "j_rate_held": jv.get("rate_held"),
                "j_rate": jv.get("target_rate"),
                # Distinguishes "measured, zero expected-status responses"
                # (achieved rate recorded — e.g. Jellyfin's login path
                # collapsing under storm load) from "leg never ran" for the
                # A1 manifest check.
                "f_achieved": hv.get("achieved_rate"), "j_achieved": jv.get("achieved_rate"),
            }
        return out, "raw-summary"
    bd = load_json(SUITE / "perf" / "bench-data.json")
    if bd and bd.get("versions"):
        latest = bd["versions"][-1]
        out = {e["name"]: {
            "f_p50": e.get("f_p50"), "f_p95": e.get("f_p95"), "f_p99": e.get("f_p99"),
            "f_rps": e.get("f_rps"), "f_ok": e.get("f_ok"),
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


def manifest_check(v2op, perf, foot, cold):
    """A1 (fail loud): every registry bench variant must have produced a latency
    row on BOTH servers, each declared special leg (TTFS copy/encode, when
    RUN_TRANSCODE=1) its footprint block, and each configured cold sentinel its
    cold row on both servers. A leg that silently vanished — the 2026-08
    transcode.js path break produced two green runs with zero TTFS rows — must
    fail the merge, never thin the record.

    Returns (skipped, missing): SKIP_VARIANTS (comma list) records a variant as
    deliberately skipped instead of missing (`cold:<name>` skips a cold row);
    anything else absent is missing.
    """
    skip = {s.strip() for s in os.environ.get("SKIP_VARIANTS", "").split(",") if s.strip()}
    missing = []
    for variant in sorted(v2op):
        if variant in skip:
            continue
        p = perf.get(variant)

        # A side is MISSING when its leg produced nothing at all — no latency
        # AND no recorded attempt. A measured window where every response was
        # unexpected-status (p50 None but achieved_rate recorded — Jellyfin's
        # login path under storm load does exactly this) is NOT a manifest
        # hole: the row lands in the record as incomparable with its 0%
        # visible. A1 exists for legs that silently vanish, not for servers
        # that fail honestly under load.
        def side_absent(prefix):
            return (p is None or (p.get(f"{prefix}_p50") is None
                                  and not p.get(f"{prefix}_achieved")))

        f_absent, j_absent = side_absent("f"), side_absent("j")
        if f_absent or j_absent:
            side = "both" if f_absent and j_absent else "ferrofin" if f_absent else "jellyfin"
            missing.append(f"{variant}[{side}]")
    if bench_env("RUN_TRANSCODE") == "1":
        for key, tgt in (("f", "ferrofin"), ("j", "jellyfin")):
            for mode in ("copy", "encode"):
                leg = f"ttfs_{mode}"
                if leg in skip:
                    continue
                if not (foot or {}).get(f"{key}_{leg}"):
                    missing.append(f"{leg}[{tgt}]")
    for name in CONFIG["BENCH_COLD_ENDPOINTS"].split():
        if f"cold:{name}" in skip:
            continue
        for tgt in ("ferrofin", "jellyfin"):
            row = (cold.get(tgt) or {}).get("endpoints", {}).get(name)
            if not row or row.get("first") is None:
                missing.append(f"cold:{name}[{tgt}]")
    return sorted(skip), sorted(missing)


BASELINE_FILE = RESULTS / "shape-baseline.json"


# Embedded diffs are capped per side so a whole-subtree divergence (hundreds of paths
# × every /Items variant) can't balloon runs.json; the count of elided paths is kept.
DIFF_PATH_CAP = 40


def shape_diff(a_paths, b_paths):
    """Key-paths present on only one side: 'missing' = in b but not a, 'extra' = in a but
    not b (each capped at DIFF_PATH_CAP with an elided-count). None when either side has
    no recorded paths (legacy hash-only captures)."""
    if a_paths is None or b_paths is None:
        return None
    a, b = set(a_paths), set(b_paths)

    def cap(paths):
        return paths[:DIFF_PATH_CAP] + (
            [f"… +{len(paths) - DIFF_PATH_CAP} more"] if len(paths) > DIFF_PATH_CAP else [])

    return {"missing": cap(sorted(b - a)), "extra": cap(sorted(a - b))}


def fp_entry(fp, variant, op):
    """Normalize one capture entry. New files key {variant_id: {hash, paths}}; legacy raw
    files keyed {op: "<hash>"} still merge (hash-only, no diffable paths)."""
    e = fp.get(variant, fp.get(op))
    if e is None:
        return None
    if isinstance(e, str):
        return {"hash": e, "paths": None}
    return e


def shape_check(f, j, base, ack):
    """The shape verdict for one GET row. Returns (reason, shape_block, baseline_entry):
    reason         — exclusion reason string, or None (row stays comparable);
    shape_block    — published on the row (cross-server match is informational);
    baseline_entry — entry to write back to the shape baseline, or None to keep the old.
    Ferrofin-vs-Jellyfin never excludes (the parity ledger owns that verdict per-op);
    Ferrofin-vs-baseline excludes until a human acks the change. Pure so the selftest
    pins it."""
    if f is None or str(f["hash"]).startswith("error:"):
        # A failed probe is NO capture, not a shape: seeding "error:HTTPError" into
        # the baseline would flag every later healthy run (and an ack would enshrine
        # the error as the reference). "non-json" stays — that IS a deterministic shape.
        return None, None, None
    if j is not None and str(j["hash"]).startswith("error:"):
        j = None  # nobody captured a Jellyfin body — no cross-server verdict to publish
    block = {"f": f["hash"]}
    if j is not None:
        block["j"] = j["hash"]
        block["matches_jellyfin"] = f["hash"] == j["hash"]
        if not block["matches_jellyfin"]:
            d = shape_diff(f.get("paths"), j.get("paths"))
            if d:
                block["diff_vs_jellyfin"] = d
    if base is None:
        block["baseline"] = "new"
        return None, block, f            # first sighting seeds the baseline
    if f["hash"] == base["hash"]:
        return None, block, None
    block["changed_since_baseline"] = True
    d = shape_diff(f.get("paths"), base.get("paths"))
    if d:
        block["diff_vs_baseline"] = d
    if ack:
        block["ack"] = True
        return None, block, f
    return ("body shape changed since baseline (review the diff, then re-merge with "
            "MERGE_ACK_SHAPES=1 to accept)"), block, None


def speedup_ratio(f_p50, j_p50):
    """The per-row speedup ratio: j/f. None when either side is missing or
    Ferrofin's median is zero (division undefined). Pure so merge_selftest.py
    pins it."""
    if f_p50 is None or j_p50 is None:
        return None
    if not f_p50:
        return None
    return round(j_p50 / f_p50, 2)


def percentile_verdicts(p):
    """Per-percentile win/loss (Ferrofin's perspective). Ferrofin <= Jellyfin is
    a win; Ferrofin > Jellyfin is a loss. Returns {'p50': 'win'|'loss', ...},
    or None when either side lacks numbers."""
    out = {}
    for pct in ("p50", "p95", "p99"):
        fv, j = p[f"f_{pct}"], p[f"j_{pct}"]
        if fv is None or j is None:
            return None
        out[pct] = "win" if fv <= j else "loss"
    return out


def main():
    v2op = variant_to_op()
    par = parity_by_op()
    perf, perf_src = perf_by_variant()
    fp_h = load_json(RAW / "perf-fingerprints-ferrofin.json", {})
    fp_j = load_json(RAW / "perf-fingerprints-jellyfin.json", {})
    fixtures = fixture_hash()
    # MERGE_ACK_SHAPES=1 acks every changed shape; a comma-separated variant list
    # acks selectively (so an intended change can be accepted without also
    # admitting an unexplained one that landed in the same run).
    ack_env = os.environ.get("MERGE_ACK_SHAPES", "")
    ack_all = ack_env.strip() == "1"
    ack_set = set() if ack_all else {v.strip() for v in ack_env.split(",") if v.strip()}
    baseline = load_json(BASELINE_FILE)
    baseline_reset = None
    if baseline and baseline.get("fixture_hash") != fixtures:
        # Different fixtures shape different bodies — comparing to this baseline would flag
        # every row for the wrong reason. Informational-only this run; reseed loudly.
        baseline_reset = f"fixture changed {baseline.get('fixture_hash')} → {fixtures}"
        baseline = None
    baseline_updates, acked = {}, []
    foot = footprint()
    perf_meta = (load_json(SUITE / "perf/results/raw/ferrofin-summary.json") or {}).get("meta")
    cold = {tgt: load_json(SUITE / f"perf/results/raw/{tgt}-cold-requests.json", {})
            for tgt in ("ferrofin", "jellyfin")}

    # A1: measure the full manifest or fail loud (no green record with holes).
    # MERGE_ALLOW_INCOMPLETE=1 downgrades to a record stamped `incomplete` that
    # is written but kept OUT of the trend file (needed for the legacy
    # bench-data fallback, which predates the full endpoint set).
    skipped, missing = manifest_check(v2op, perf, foot, cold)
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
        f_e = None if is_write else fp_entry(fp_h, variant, op)
        j_e = None if is_write else fp_entry(fp_j, variant, op)
        base_e = ((baseline or {}).get("variants") or {}).get(variant)
        shape_reason, shape_block, base_update = shape_check(
            f_e, j_e, base_e, ack_all or variant in ack_set)
        if base_update is not None:
            baseline_updates[variant] = base_update
        if shape_block and shape_block.get("ack"):
            acked.append(variant)
        both_ok = p.get("f_ok") == 100 and p.get("j_ok") == 100
        have_lat = p["f_p50"] is not None and p["j_p50"] is not None
        # G1: a latency comparison is only meaningful at the same arrival rate,
        # actually held on both sides. Legacy (closed-loop) records carry no
        # rate keys — None == None keeps them flowing through unchanged.
        same_rate = p.get("rate") == p.get("j_rate")
        rates_held = p.get("f_rate_held") is not False and p.get("j_rate_held") is not False
        comparable = deep and both_ok and have_lat and not shape_reason and same_rate and rates_held
        reason = (None if comparable else
                  shape_reason if shape_reason else
                  "not deep-verified" if not deep else
                  "200-rate < 100%" if not both_ok else
                  "measured at different arrival rates" if not same_rate else
                  "open-loop rate not held" if not rates_held else "missing latency")

        verdicts = percentile_verdicts(p)
        vlist = list(verdicts.values()) if verdicts else []
        win = vlist.count("win") == 3
        speedup = speedup_ratio(p["f_p50"], p["j_p50"])
        # H2: cold rows ride the same operation, as a separate labeled block —
        # WARM percentiles above are the headline; cold is published beside
        # them, never blended (fresh-process first-request latency).
        ch = (cold.get("ferrofin") or {}).get("endpoints", {}).get(variant)
        cj = (cold.get("jellyfin") or {}).get("endpoints", {}).get(variant)
        cold_block = None
        if ch or cj:
            cold_block = {
                "f_first": (ch or {}).get("first"), "f_p50": (ch or {}).get("p50"),
                "f_max": (ch or {}).get("max"), "f_ready_ms": (ch or {}).get("ready_wait_ms"),
                "j_first": (cj or {}).get("first"), "j_p50": (cj or {}).get("p50"),
                "j_max": (cj or {}).get("max"), "j_ready_ms": (cj or {}).get("ready_wait_ms"),
            }
        operations.append({
            "op": op, "tag": tag,
            # Body-shape record: Ferrofin's hash, Jellyfin's hash + match (informational),
            # and — when changed vs the reviewed baseline — the field-level diff.
            **({"shape": shape_block} if shape_block else {}),
            # E2: core vs compiled-in-extension ownership, from the parity
            # ledger (whose source is the EXTENSION_ROUTES const in
            # ferrofin-api — compile-time asserted against REAL_ROUTES).
            "owner": pr.get("owner", "core"),
            "parity": {"depth": pr.get("depth"), "deep_verified": deep,
                       "classification": pr.get("classification") or None},
            "perf": {"variant": variant, **p, "speedup": speedup,
                     "win_all_three": win,
                     "verdicts": verdicts,
                     "tail_loss": bool(comparable and verdicts
                                       and verdicts["p50"] == "win"
                                       and "loss" in (verdicts["p95"], verdicts["p99"])),
                     "comparable": comparable, "reason": reason,
                     **({"cold": cold_block} if cold_block else {})},
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
    # E3: per-owner breakdown — extensions must not dilute or flatter core's
    # numbers, so each owner's share stands alone. (All benched variants are
    # core today; extension variants slot in with no further edits here.)
    owners = {}
    for o in operations:
        ow = owners.setdefault(o["owner"], {"rows": 0, "comparable_rows": 0,
                                            "speedups": [], "wins": 0, "deep": 0})
        ow["rows"] += 1
        ow["deep"] += bool(o["parity"]["deep_verified"])
        if o["perf"]["comparable"]:
            ow["comparable_rows"] += 1
            ow["wins"] += bool(o["perf"]["win_all_three"])
            if o["perf"]["speedup"] is not None:
                ow["speedups"].append(o["perf"]["speedup"])
    owners = {
        name: {
            "rows": ow["rows"],
            "comparable_rows": ow["comparable_rows"],
            "median_speedup": round(median(ow["speedups"]), 3) if ow["speedups"] else None,
            "win_rate": round(ow["wins"] / ow["comparable_rows"], 3) if ow["comparable_rows"] else None,
            "parity_coverage": round(ow["deep"] / ow["rows"], 3) if ow["rows"] else None,
        }
        for name, ow in owners.items()
    }

    headline = {
        "comparable_rows": len(comp),
        "dropped_rows": sum(dropped.values()),
        "dropped_by_reason": dropped,
        # Cross-server shape divergences are PUBLISHED, not hidden in exclusions: rows whose
        # body shape differs from Jellyfin's at bench time (see each row's `shape` block —
        # the parity ledger classifies whether each is a defect or a documented divergence).
        "shape_divergences_vs_jellyfin": sum(
            1 for o in operations if o.get("shape", {}).get("matches_jellyfin") is False),
        "median_speedup": round(median(speedups), 3) if speedups else None,
        "win_rate": round(sum(o["win_all_three"] for o in comp) / len(comp), 3) if comp else None,
        "tail_losses": [o["variant"] for o in comp if o["tail_loss"]],
        "parity_coverage": round(len(deep_ops) / len(benched_ops), 3) if benched_ops else None,
        "owners": owners,
    }

    sha = sh("git", "rev-parse", "--short", "HEAD") or "unknown"
    record = {
        "meta": {
            "ferrofin": sh("git", "describe", "--tags", "--always") or "dev",
            "ferrofin_sha": sha,
            "jellyfin_image": bench_env("JELLYFIN_IMAGE") or "jellyfin/jellyfin:10.11.8",
            "fixture_hash": fixtures,
            "cpus": int(bench_env("BENCH_CPUS")) if bench_env("BENCH_CPUS") else None,
            "mem": bench_env("BENCH_MEM"),
            # The load model + engine + resolved methodology knobs come from the
            # perf leg's own summary meta (compare.py is the ONLY producer of
            # that meta block, and only it runs open-loop) — a summary without
            # it (the legacy bench-data fallback, or a stale k6 raw summary)
            # must NOT be stamped open-loop: gate.py's rebaseline honesty
            # check keys on this (review finding, round 1).
            "load": {"model": "open-loop" if perf_meta else None,
                     "duration_secs": (perf_meta or {}).get("bench_config", {})
                     .get("values", {}).get("BENCH_DURATION_SECS")},
            "engine": (perf_meta or {}).get("engine"),
            "ping_ceiling_rps": (perf_meta or {}).get("ping_ceiling_rps"),
            "bench_config": (perf_meta or {}).get("bench_config"),
            "perf_source": perf_src,
            "footprint": foot,
            # B1: the build identity the running server reported over
            # /health/live during the perf leg (run.sh verified it against the
            # tree before measuring); None for records that predate the check.
            "server_build": server_build(),
            "skipped_variants": skipped or None,
            "incomplete": missing or None,
            **({"shape_baseline_reset": baseline_reset} if baseline_reset else {}),
            **({"shape_acked": sorted(acked)} if acked else {}),
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
        # Numbered so successive incomplete runs of one SHA don't overwrite
        # each other's evidence (review carry-over, round 2).
        seq = 1
        while (RESULTS / f"run-{sha}-incomplete-{seq}.json").exists():
            seq += 1
        out = RESULTS / f"run-{sha}-incomplete-{seq}.json"
        record["meta"]["run_label"] = f"{record['meta']['ferrofin']} (incomplete)"
        out.write_text(json.dumps(record, indent=2) + "\n")
        print(f">> wrote {out.relative_to(ROOT)} — INCOMPLETE, excluded from the trend")
        return

    # The shape baseline advances only on a COMPLETE record, and only when something
    # actually moved (a seed, an ack, or a fixture reset): unchanged runs must leave
    # the file byte-identical, so `git diff suite/results/shape-baseline.json` IS the
    # body-shape change under review — a file that churned on every gate run would
    # train people to blind-commit it. Variants dropped from the registry are pruned.
    if fp_h and (baseline_updates or baseline_reset):
        merged = dict(((baseline or {}).get("variants") or {}))
        merged.update(baseline_updates)
        known = {v for v, _ in v2op.items()}
        merged = {v: e for v, e in merged.items() if v in known}
        BASELINE_FILE.write_text(json.dumps(
            {"fixture_hash": fixtures, "sha": sha, "variants": merged},
            indent=2, sort_keys=True) + "\n")

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
    if hl["shape_divergences_vs_jellyfin"]:
        print(f"   shape ≠ jellyfin on {hl['shape_divergences_vs_jellyfin']} rows "
              "(informational — each row's shape.diff_vs_jellyfin names the fields)")
    changed = [o["perf"]["variant"] for o in operations
               if (o.get("shape") or {}).get("changed_since_baseline")
               and not (o.get("shape") or {}).get("ack")]
    if changed:
        print(f"   !! body shape CHANGED since baseline — rows excluded until reviewed "
              f"(MERGE_ACK_SHAPES=1): {', '.join(changed)}")
    unmatched = ack_set - set(acked)
    if unmatched:
        # A typo'd ack name would otherwise be silently ignored while its row stays excluded.
        print(f"   !! MERGE_ACK_SHAPES named variants with no shape change to ack: "
              f"{', '.join(sorted(unmatched))}")


if __name__ == "__main__":
    main()
