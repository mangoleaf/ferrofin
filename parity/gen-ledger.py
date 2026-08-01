#!/usr/bin/env python3
"""Phase 0 of the parity-verification plan: generate the per-operation parity ledger.

Enumerates every operation in the vendored Jellyfin OpenAPI contract
(all 412 ops) and emits one ledger row each into `parity/ledger.json`, then
renders the human dashboard `parity/LEDGER.md` with the headline number.

Signals wired now (Phase 0):
  route    registered / 501-stub   <- hermit-api handlers::REAL_ROUTES
  depth    REAL / PARTIAL / HOLLOW / STUB   <- REAL_ROUTES + optional classify.tsv overlay
  deep_verified / classification / last_verified   <- parity/seed.json (fix-loop results)

Columns left null are untested and are filled by later layers (contract sweep =
status_conformant/schema_valid; differential replay = deep_verified at scale).

Run from the repo root:
  parity/gen-ledger.py                 # emit ledger.json + LEDGER.md
  parity/gen-ledger.py classify.tsv    # overlay HOLLOW/PARTIAL depth from an audit
"""
import glob
import json
import os
import re
import sys
from collections import defaultdict

METHODS = ("get", "post", "put", "delete", "patch", "head")
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def norm(method, path):
    """Param-name-agnostic key matching REAL_ROUTES/seed to spec paths.

    Mirrors the router's `to_axum_path`: any segment containing a `{placeholder}`
    collapses to `{}`; literal-only segments are kept verbatim. Copied from the
    api-status skill's scan.py (the origin of this normalization).
    """
    segments = ["{}" if "{" in seg else seg for seg in path.split("/")]
    return (method.lower(), "/".join(segments))


def load_spec():
    files = sorted(glob.glob(os.path.join(ROOT, "contracts/jellyfin-openapi-*.json")))
    if not files:
        sys.exit("no contracts/jellyfin-openapi-*.json")
    return json.load(open(files[-1]))


def load_real_routes():
    mod = open(os.path.join(ROOT, "crates/hermit-api/src/handlers/mod.rs")).read()
    blk = mod[mod.index("pub const REAL_ROUTES"):]
    blk = blk[: blk.index("\n];")]
    pairs = re.findall(r'\(\s*"(\w+)"\s*,\s*"([^"]+)"\s*,?\s*\)', blk)
    return set(norm(m, p) for m, p in pairs)


def load_overlay(path):
    overlay = {}
    if path:
        for line in open(path):
            line = line.rstrip("\n")
            if line and not line.startswith("#"):
                state, method, p = line.split("\t")
                overlay[norm(method, p)] = state
    return overlay


def load_seed():
    seed_path = os.path.join(ROOT, "parity/seed.json")
    if not os.path.exists(seed_path):
        return {}, None
    seed = json.load(open(seed_path))
    by_key = {}
    for row in seed["rows"]:
        method, _, p = row["operation"].partition(" ")
        by_key[norm(method, p)] = row
    return by_key, seed.get("last_verified")


def load_journeys():
    """Layer-2 write-journey results — feed deep_verified/classification for write ops."""
    path = os.path.join(ROOT, "parity/journey-results.json")
    if not os.path.exists(path):
        return {}, None
    data = json.load(open(path))
    by_key = {norm(*op.split(" ", 1)): r for op, r in data["rows"].items()}
    return by_key, data.get("last_verified") or None


def load_sweep():
    path = os.path.join(ROOT, "parity/sweep-results.json")
    if not os.path.exists(path):
        return {}
    rows = json.load(open(path))["rows"]
    return {norm(*op.split(" ", 1)): r for op, r in rows.items()}


def build_rows(spec, real, overlay, curated, sweep):
    rows = []
    for path, item in spec["paths"].items():
        for method, op in item.items():
            if method not in METHODS:
                continue
            k = norm(method, path)
            depth = overlay.get(k) or ("REAL" if k in real else "STUB")
            s = curated.get(k, {})
            sw = sweep.get(k, {})
            rows.append({
                "operation": f"{method.upper()} {path}",
                "tag": (op.get("tags") or ["_untagged"])[0],
                "route": "registered" if k in real else "501-stub",
                "depth": depth,
                "status_conformant": sw.get("status_conformant"),
                "schema_valid": sw.get("schema_valid"),
                "note": sw.get("note", ""),
                "deep_verified": s.get("deep_verified"),
                "classification": s.get("classification", ""),
                "last_verified": s.get("last_verified") if s else None,
            })
    rows.sort(key=lambda r: (r["operation"].split(" ", 1)[1], r["operation"]))
    return rows


def render_md(rows):
    total = len(rows)
    deep = sum(1 for r in rows if r["deep_verified"] is True)
    classified = sum(1 for r in rows if r["classification"] and r["deep_verified"] is not True)
    untested = sum(1 for r in rows if r["deep_verified"] is None and not r["classification"])
    depth_counts = defaultdict(int)
    route_counts = defaultdict(int)
    for r in rows:
        depth_counts[r["depth"]] += 1
        route_counts[r["route"]] += 1

    pct = lambda n: f"{100 * n // total}%"
    out = []
    out.append("# Hermit ⇄ Jellyfin parity ledger\n")
    out.append("_Generated by `parity/gen-ledger.py` — do not hand-edit; edit `parity/seed.json` "
               "or the classify overlay and regenerate._\n")
    sc_yes = sum(1 for r in rows if r["status_conformant"] is True)
    sc_run = sum(1 for r in rows if r["status_conformant"] is not None)
    sv_yes = sum(1 for r in rows if r["schema_valid"] is True)
    sv_run = sum(1 for r in rows if r["schema_valid"] is not None)
    layer1 = (f"Layer 1: {sc_yes}/{sc_run} status-conformant · {sv_yes}/{sv_run} schema-valid"
              if sc_run or sv_run else "status-conformance + schema-validation not yet run — Layer 1")
    out.append(f"**{deep}/{total} deep-verified · {classified} classified-divergence · "
               f"{untested} untested**  \n_{layer1}_\n")
    out.append("## Depth (what the wired handler actually does)\n")
    out.append("| depth | ops | % |")
    out.append("|---|---:|---:|")
    for k in ("REAL", "PARTIAL", "HOLLOW", "STUB"):
        out.append(f"| {k} | {depth_counts[k]} | {pct(depth_counts[k])} |")
    out.append(f"| **route registered** | {route_counts['registered']} | {pct(route_counts['registered'])} |")
    out.append(f"| **route 501-stub** | {route_counts['501-stub']} | {pct(route_counts['501-stub'])} |")
    out.append("")
    out.append("## Deep-verified (response + read-back diffed clean vs Jellyfin 10.11.8)\n")
    for r in rows:
        if r["deep_verified"] is True:
            out.append(f"- ✅ `{r['operation']}`")
    out.append("")
    out.append("## Classified divergence (accepted — not a bug)\n")
    for r in rows:
        if r["classification"] and r["deep_verified"] is not True:
            out.append(f"- ⚠️ `{r['operation']}` — {r['classification']}")
    out.append("")
    out.append("## Full ledger\n")
    out.append("_deep/status/schema: ✅ pass · ⚠️ fail · · untested_\n")
    out.append("| operation | route | depth | status | schema | deep | classification |")
    out.append("|---|---|---|---|---|---|---|")
    mark = {True: "✅", False: "⚠️", None: "·"}
    for r in rows:
        out.append(f"| `{r['operation']}` | {r['route']} | {r['depth']} | "
                   f"{mark[r['status_conformant']]} | {mark[r['schema_valid']]} | "
                   f"{mark[r['deep_verified']]} | {r['classification']} |")
    out.append("")
    return "\n".join(out)


def build_curated():
    """Merge curated deep-verification: seed.json (reads) + journey-results.json (writes),
    each row carrying its own last_verified stamp."""
    seed, seed_stamp = load_seed()
    journeys, j_stamp = load_journeys()
    curated = {k: {**v, "last_verified": seed_stamp} for k, v in seed.items()}
    for k, v in journeys.items():
        curated[k] = {**v, "last_verified": j_stamp}
    return curated


def check(rows, curated):
    """Self-check: the ledger must cover 412 ops and every curated row must match one
    (a typo'd path silently drops its classification otherwise)."""
    assert len(rows) == 412, f"expected 412 ops, got {len(rows)}"
    keys = {norm(*r["operation"].split(" ", 1)) for r in rows}
    unmatched = [k for k in curated if k not in keys]
    assert not unmatched, f"curated rows match no spec op: {unmatched}"
    print(f"ok: {len(rows)} ops, all {len(curated)} curated rows matched")


def main():
    spec = load_spec()
    real = load_real_routes()
    curated = build_curated()
    sweep = load_sweep()

    if "--check" in sys.argv:
        check(build_rows(spec, real, {}, curated, sweep), curated)
        return

    classify_path = next((a for a in sys.argv[1:] if not a.startswith("--")), None)
    overlay = load_overlay(classify_path)
    rows = build_rows(spec, real, overlay, curated, sweep)

    with open(os.path.join(ROOT, "parity/ledger.json"), "w") as f:
        json.dump({"operations": rows}, f, indent=2)
        f.write("\n")
    with open(os.path.join(ROOT, "parity/LEDGER.md"), "w") as f:
        f.write(render_md(rows))

    deep = sum(1 for r in rows if r["deep_verified"] is True)
    print(f"wrote parity/ledger.json + parity/LEDGER.md — {len(rows)} ops, {deep} deep-verified")


if __name__ == "__main__":
    main()
