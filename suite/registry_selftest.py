#!/usr/bin/env python3
"""Self-test for suite/registry.json — the spirit of contract_superset.rs, for the bench side.

Asserts (Plan 6, "One registry"):
  1. every variant's `op` exists as a GET in the vendored OpenAPI spec,
  2. no duplicate variant ids (ids are permanent trend keys),
  3. every alias (`was`) is unique and does not shadow a live id (so history joins survive).

Run: python3 suite/registry_selftest.py   (exit 0 = green). No test framework by design.
"""
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SPEC = next((ROOT / "contracts").glob("jellyfin-openapi-*.json"))
REGISTRY = Path(__file__).resolve().parent / "registry.json"


def main():
    spec_paths = json.loads(SPEC.read_text())["paths"]
    ops = json.loads(REGISTRY.read_text())["operations"]

    errors, ids, aliases = [], set(), set()
    for entry in ops:
        method, _, path = entry["op"].partition(" ")
        if path not in spec_paths or method.lower() not in spec_paths[path]:
            errors.append(f"op not in spec: {entry['op']}")
        for v in entry["variants"]:
            vid = v["id"]
            if vid in ids:
                errors.append(f"duplicate variant id: {vid}")
            ids.add(vid)
            if "was" in v:
                if v["was"] in aliases:
                    errors.append(f"duplicate alias: {v['was']}")
                aliases.add(v["was"])

    for a in aliases & ids:
        errors.append(f"alias shadows a live id: {a}")

    if errors:
        print("registry self-test FAILED:", file=sys.stderr)
        for e in errors:
            print(f"  - {e}", file=sys.stderr)
        sys.exit(1)
    print(f">> registry self-test OK: {len(ops)} operations, {len(ids)} variants, "
          f"{len(aliases)} aliases")


if __name__ == "__main__":
    main()
