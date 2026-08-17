#!/usr/bin/env python3
"""The login storm — deliberately the very LAST leg of a bench.

PBKDF2 saturates CPU, every login invalidates the server-side auth cache, and
Jellyfin's brute-force limiter can lock the bench user for a while after the
storm — anything measured after it is poisoned (observed live: every
post-storm cold probe failed auth). run.sh therefore runs this after the main
windows, TTFS, fingerprints AND the cold leg; nothing measures after it.

Updates the target's existing summary (results/raw/<target>-summary.json)
with the auth_login row. Jellyfin routinely fails most/all storm logins (its
limiter treats the storm as an attack — a long-known behavior); that lands as
an honest 0-ok incomparable row, never a manifest hole.
"""

import argparse
import json
import sys

from bootstrap import RAW, load_ctx
from compare import open_loop_window
from config import CONFIG
from endpoints import ENDPOINTS


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--target", required=True, choices=["ferrofin", "jellyfin"])
    ap.add_argument("--base", required=True)
    args = ap.parse_args()

    ctx = load_ctx(args.target)
    if ctx is None:
        sys.exit(f"no {args.target}-ctx.json — run the main leg (compare.py) first")
    summary_path = RAW / f"{args.target}-summary.json"
    try:
        out = json.loads(summary_path.read_text())
    except OSError:
        sys.exit(f"no {summary_path.name} — run the main leg (compare.py) first")

    login = next(e for e in ENDPOINTS if e["scenario"] == "login")
    rate = CONFIG["BENCH_LOGIN_RATE"]
    secs = CONFIG["BENCH_LOGIN_DURATION_SECS"]
    row = open_loop_window(args.base, login, ctx, rate, 0, secs)
    row["rate_source"] = "login"
    row["rate_held"] = row["achieved_rate"] >= CONFIG["BENCH_RATE_TOLERANCE"] * rate
    out["endpoints"]["auth_login"] = row
    summary_path.write_text(json.dumps(out, indent=2) + "\n")
    print(f"   login storm: p50={row['p50']} ok={row['okPct']}%")
    if not row["rate_held"]:
        sys.exit(f"login storm: achieved {row['achieved_rate']}/s of target {rate}/s — rate not held")


if __name__ == "__main__":
    main()
