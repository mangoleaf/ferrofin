#!/usr/bin/env python3
"""Show what Jellyfin returned next to what Ferrofin returned, for any endpoint.

    suite/parity/show.py                      # every endpoint that differs, one line each
    suite/parity/show.py --all                # every captured endpoint
    suite/parity/show.py /Items               # side-by-side for matching routes
    suite/parity/show.py "GET /Sessions" -v   # full pair, both bodies

Reads suite/parity/samples.json, written by the parity leg. Without it, run
`suite/run.sh parity` — the samples are captured during the body diff.
"""
import json
import sys
from pathlib import Path

SAMPLES = Path(__file__).resolve().parent / "samples.json"
G, R, Y, B, D = "\033[32m", "\033[31m", "\033[33m", "\033[1m", "\033[0m"


def load():
    try:
        return json.loads(SAMPLES.read_text())
    except OSError:
        sys.exit(f"no {SAMPLES} — run `suite/run.sh parity` to capture samples")


# THE SAME diff engine that produced the verdict — never a second implementation.
# A naive index-wise flatten pairs element 0 with element 0, so two servers whose
# lists are ordered differently report every field as a difference (observed: a
# music track diffed against a TV channel, 44 "differences", zero of them real).
# parity_diff aligns arrays by a stable key (Path > Name > Id) and skips the
# volatile keys that legitimately differ between instances.
from parity_diff import diff as _diff, VOLATILE  # noqa: E402


def diff_pair(jellyfin, ferrofin):
    """-> (buckets, compared). Buckets are mismatch/missing/extra, each entry
    carrying the aligned path and both sides' values."""
    out = {"mismatch": [], "missing": [], "extra": []}
    stats = {"compared": 0}
    _diff(jellyfin, ferrofin, "", out, VOLATILE, stats)
    return out, stats.get("compared", 0)


def compare(name, s, verbose=False):
    buckets, compared = diff_pair(s["jellyfin"], s["ferrofin"])
    n = sum(len(v) for v in buckets.values())
    print(f"\n{B}{name}{D}   {compared} fields compared, {R if n else G}{n} differ{D}"
          f"   {D}(volatile keys skipped; arrays aligned by Path/Name/Id)")
    if not n:
        print(f"  {G}identical{D}")
    for kind, col, label in (("missing", R, "Jellyfin has, Ferrofin does NOT"),
                             ("extra", Y, "Ferrofin has, Jellyfin does NOT"),
                             ("mismatch", Y, "both have, values differ")):
        rows = buckets[kind]
        if not rows:
            continue
        print(f"  {col}{label} ({len(rows)}){D}")
        for e in rows[:40]:
            print(f"    {e.get('path')}")
            if kind != "missing":
                print(f"        ferrofin: {json.dumps(e.get('h'))[:150]}")
            if kind != "extra":
                print(f"        jellyfin: {json.dumps(e.get('j'))[:150]}")
        if len(rows) > 40:
            print(f"    ... +{len(rows) - 40} more")
    if verbose:
        print(f"  {B}--- full jellyfin ---{D}\n{json.dumps(s['jellyfin'], indent=2)[:4000]}")
        print(f"  {B}--- full ferrofin ---{D}\n{json.dumps(s['ferrofin'], indent=2)[:4000]}")


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("-")]
    verbose = "-v" in sys.argv or "--verbose" in sys.argv
    every = "--all" in sys.argv
    doc = load()
    samples = doc["samples"]
    print(f"{len(samples)} endpoint pairs · limits {doc.get('limits')}")

    if args:
        q = args[0].lower()
        hit = {k: v for k, v in samples.items() if q in k.lower() or q in (v.get("route") or "").lower()}
        if not hit:
            sys.exit(f"no captured endpoint matches {args[0]!r}")
        for k, v in hit.items():
            compare(k, v, verbose)
        return

    # summary: one line per endpoint, differing ones first
    rows = []
    for k, v in samples.items():
        buckets, compared = diff_pair(v["jellyfin"], v["ferrofin"])
        rows.append((sum(len(x) for x in buckets.values()), compared, k))
    rows.sort(key=lambda r: (-r[0], r[2]))
    for nd, nk, k in rows:
        if nd == 0 and not every:
            continue
        print(f"  {R if nd else G}{nd:4} differ{D} / {nk:4} compared   {k}")
    same = sum(1 for r in rows if r[0] == 0)
    print(f"\n{same} identical, {len(rows) - same} differing"
          f"{'' if every else '   (--all to list the identical ones too)'}")
    print("suite/parity/show.py '<route>' for the field-by-field pair")


if __name__ == "__main__":
    main()
