#!/usr/bin/env python3
"""Deterministic core of the api-status skill.

Enumerates every operation in the vendored Jellyfin OpenAPI contract
(`contracts/jellyfin-openapi-*.json`) and joins it against hermit-api's
`REAL_ROUTES` table to classify each op:

  REAL  - a real handler is registered (router skips the shared 501 stub)
  STUB  - no real handler; falls through to the 501 stub

CAUTION: `REAL_ROUTES` membership only proves "a handler returns 200", NOT that
it returns real data. A handler can be REGISTERED yet HOLLOW (returns a constant
`Json(X::default())` regardless of input) or call a stub-injected manager. Those
states can only be found by reading handler bodies + the composition root; the
skill's agent-audit step produces a classification TSV that this script overlays.

Usage (run from the hermit repo root):
  scan.py                       # REAL vs STUB baseline table
  scan.py classify.tsv          # overlay HOLLOW/PARTIAL from the audit
  scan.py classify.tsv --list   # also dump the STUB + HOLLOW op lists

classify.tsv lines: STATE<TAB>method<TAB>/Path   (STATE = HOLLOW or PARTIAL)
"""
import glob
import json
import re
import sys
from collections import defaultdict

args = sys.argv[1:]
want_list = "--list" in args
positional = [a for a in args if not a.startswith("--")]
classify_path = positional[0] if positional else None

METHODS = ("get", "post", "put", "delete", "patch", "head")


def norm(method, path):
    """Param-name-agnostic key that matches REAL_ROUTES to spec paths.

    Replicates the router's `to_axum_path` (crates/hermit-api/src/routes.rs): any
    segment containing a `{placeholder}` collapses to a single `{}` capture of the
    whole segment, so a mixed literal+param segment folds to `{}` too
    (`stream.{container}` -> `{}`, `Stream.{routeFormat}` -> `{}`,
    `{segmentId}.{container}` -> `{}`). Literal-only segments are kept verbatim
    (`subtitles.m3u8`, `stream`, `Countries`), and param names are irrelevant.
    This is exactly the equivalence the router keys on, so a route registered
    under Hermit's spelling matches the vendored spec path it serves.
    """
    segments = ["{}" if "{" in seg else seg for seg in path.split("/")]
    return (method, "/".join(segments))


def load_spec():
    files = sorted(glob.glob("contracts/jellyfin-openapi-*.json"))
    if not files:
        sys.exit("no contracts/jellyfin-openapi-*.json (run from the repo root)")
    return json.load(open(files[-1]))


def load_real_routes():
    mod = open("crates/hermit-api/src/handlers/mod.rs").read()
    start = mod.index("pub const REAL_ROUTES")
    blk = mod[start:]
    blk = blk[: blk.index("\n];")]
    # Tolerate a trailing comma before `)` so multi-line entries (long paths
    # split across lines with a dangling comma) are captured, not just one-liners.
    pairs = re.findall(r'\(\s*"(\w+)"\s*,\s*"([^"]+)"\s*,?\s*\)', blk)
    return set(norm(m, p) for m, p in pairs)


def load_overlay(path):
    overlay = {}
    if not path:
        return overlay
    for line in open(path):
        line = line.rstrip("\n")
        if not line or line.startswith("#"):
            continue
        state, method, p = line.split("\t")
        overlay[norm(method, p)] = state
    return overlay


def main():
    spec = load_spec()
    real = load_real_routes()
    overlay = load_overlay(classify_path)

    counts = defaultdict(lambda: defaultdict(int))
    stubs = defaultdict(list)
    hollow = defaultdict(list)
    for path, item in spec["paths"].items():
        for method, op in item.items():
            if method not in METHODS:
                continue
            tag = (op.get("tags") or ["_untagged"])[0]
            k = norm(method, path)
            state = overlay.get(k) or ("REAL" if k in real else "STUB")
            counts[tag][state] += 1
            if state == "STUB":
                stubs[tag].append(f"{method.upper()} {path}")
            elif state == "HOLLOW":
                hollow[tag].append(f"{method.upper()} {path}")

    g = defaultdict(int)
    print(f"{'CONTROLLER':24} {'REAL':>4} {'PART':>4} {'HOLL':>4} {'STUB':>4} {'tot':>4}")
    for tag in sorted(counts, key=lambda t: -sum(counts[t].values())):
        r, pa, h, s = (counts[tag][x] for x in ("REAL", "PARTIAL", "HOLLOW", "STUB"))
        print(f"{tag:24} {r:>4} {pa:>4} {h:>4} {s:>4} {r + pa + h + s:>4}")
        for x in ("REAL", "PARTIAL", "HOLLOW", "STUB"):
            g[x] += counts[tag][x]

    total = sum(g.values())
    print(
        f"\nTOTAL {total} ops | REAL {g['REAL']} PARTIAL {g['PARTIAL']} "
        f"HOLLOW {g['HOLLOW']} STUB {g['STUB']}"
    )
    if total:
        print(
            f"Data-backed (REAL): {100 * g['REAL'] // total}%  |  "
            f"Returns-no-real-data (HOLLOW+STUB): {100 * (g['HOLLOW'] + g['STUB']) // total}%"
        )

    if want_list:
        print("\n=== STUB (501) by controller ===")
        for tag in sorted(stubs):
            print(f"[{tag}] " + "; ".join(sorted(stubs[tag])))
        if overlay:
            print("\n=== HOLLOW by controller ===")
            for tag in sorted(hollow):
                print(f"[{tag}] " + "; ".join(sorted(hollow[tag])))


if __name__ == "__main__":
    main()
