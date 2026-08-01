#!/usr/bin/env python3
"""Parse every persisted benchmark report (results/v*.md) into one JSON the
viewer (index.html) loads. Mirrors parity/gen-ledger.py: stdlib only, no deps.

Each results/<version>.md is the canonical per-version artifact run.sh writes;
this collects them so index.html can compare any two versions side by side."""

import glob
import json
import os
import re

HERE = os.path.dirname(os.path.abspath(__file__))
RESULTS = os.path.join(HERE, "results")

# `| `name` | h50 / h95 / h99 | j50 / j95 / j99 | hrps vs jrps | hok% / jok% | speedup |`
ROW = re.compile(
    r"^\|\s*`([^`]+)`\s*\|"
    r"\s*([\d.]+)\s*/\s*([\d.]+)\s*/\s*([\d.]+)\s*\|"
    r"\s*([\d.]+)\s*/\s*([\d.]+)\s*/\s*([\d.]+)\s*\|"
    r"\s*([\d.]+)\s*vs\s*([\d.]+)\s*\|"
    r"\s*([\d.]+)%\s*/\s*([\d.]+)%\s*\|"
    r"\s*([\d.]+)x?\s*\|"
)
HDR = {
    "hermit": re.compile(r"\*\*Hermit:\*\*\s*`([^`]+)`"),
    "jellyfin": re.compile(r"\*\*Jellyfin:\*\*\s*`([^`]+)`"),
    "when": re.compile(r"\*\*When:\*\*\s*(.+)"),
    "host": re.compile(r"\*\*Host:\*\*\s*(.+)"),
    "library": re.compile(r"\*\*Library:\*\*\s*(.+)"),
}
# A generic 3-column footprint row (label wording drifts between versions).
FOOT = re.compile(r"^\|\s*(.+?)\s*\|\s*(.+?)\s*\|\s*(.+?)\s*\|\s*$")


def num(s):
    try:
        return float(s)
    except ValueError:
        return None


def parse(path):
    text = open(path, encoding="utf-8").read()
    v = {"file": os.path.basename(path), "endpoints": [], "footprint": []}
    for key, rx in HDR.items():
        m = rx.search(text)
        v[key] = m.group(1) if m else ""

    in_foot = False
    for line in text.splitlines():
        m = ROW.match(line)
        if m:
            g = m.groups()
            v["endpoints"].append(
                {
                    "name": g[0],
                    "h_p50": num(g[1]), "h_p95": num(g[2]), "h_p99": num(g[3]),
                    "j_p50": num(g[4]), "j_p95": num(g[5]), "j_p99": num(g[6]),
                    "h_rps": num(g[7]), "j_rps": num(g[8]),
                    "h_ok": num(g[9]), "j_ok": num(g[10]),
                    "speedup": num(g[11]),
                }
            )
            continue
        if line.startswith("## Footprint"):
            in_foot = True
            continue
        if in_foot:
            fm = FOOT.match(line)
            if not fm:
                continue
            label, herm, jelly = (c.strip() for c in fm.groups())
            if label in ("Metric", "") or set(label) <= {"-", ":"}:
                continue  # header / separator row
            v["footprint"].append({"metric": label, "hermit": herm, "jellyfin": jelly})
    return v


def main():
    files = sorted(glob.glob(os.path.join(RESULTS, "v*.md")))
    versions = [parse(f) for f in files]
    versions = [v for v in versions if v["endpoints"]]
    # Chronological by report timestamp; the viewer defaults to the last two.
    versions.sort(key=lambda v: v["when"])
    out = os.path.join(HERE, "bench-data.json")
    with open(out, "w", encoding="utf-8") as f:
        json.dump({"versions": versions}, f, indent=1)
    print(f"wrote {out} — {len(versions)} versions")


if __name__ == "__main__":
    main()
