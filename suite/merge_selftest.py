#!/usr/bin/env python3
"""Self-check for merge.py's pure comparison logic (bats-invoked, no data files).

Covers the win/loss verdict (no ties — Ferrofin <= Jellyfin is a win) and the
A1 manifest checker's missing/skip accounting.
"""
import os

# Resolved at config-import time: keep the cold/TTFS expectations out of this
# pure-logic check (the bats fail-loud tests cover those manifest legs).
os.environ["BENCH_COLD_ENDPOINTS"] = ""
os.environ["RUN_TRANSCODE"] = "0"

import merge  # noqa: E402


def row(f50, f95, f99, j50, j95, j99):
    return {"f_p50": f50, "f_p95": f95, "f_p99": f99,
            "j_p50": j50, "j_p95": j95, "j_p99": j99}


# Clear win on all three (Ferrofin lower).
assert merge.percentile_verdicts(row(10, 20, 30, 20, 40, 60)) == \
    {"p50": "win", "p95": "win", "p99": "win"}
# Sub-ms values: still wins — 0.11 vs 0.99 is a real speedup.
assert merge.percentile_verdicts(row(1.0, 2.0, 3.0, 2.0, 3.5, 5.0)) == \
    {"p50": "win", "p95": "win", "p99": "win"}
# p50 win, p99 loss — the tail-loss shape.
v = merge.percentile_verdicts(row(10, 20, 100, 20, 21, 50))
assert v == {"p50": "win", "p95": "win", "p99": "loss"}
# Small delta on above-zero values: still a win.
assert merge.percentile_verdicts(row(10, 10, 10, 13, 13, 13))["p50"] == "win"
# Equal values: win (Ferrofin is at least as fast).
assert merge.percentile_verdicts(row(5, 5, 5, 5, 5, 5)) == \
    {"p50": "win", "p95": "win", "p99": "win"}
# Missing numbers on either side → None (row can't be judged).
assert merge.percentile_verdicts(row(None, 20, 30, 20, 40, 60)) is None

# The speedup ratio — straightforward j/f.
assert merge.speedup_ratio(100, 300) == 3.0
assert merge.speedup_ratio(0.1, 0.3) == 3.0     # sub-ms: still computed
assert merge.speedup_ratio(10, 13) == 1.3
assert merge.speedup_ratio(None, 300) is None
assert merge.speedup_ratio(100, None) is None
assert merge.speedup_ratio(0, 1) is None         # zero: ratio undefined
assert merge.speedup_ratio(0, 300) is None

# Manifest checker: a variant missing on one side is reported with its side;
# SKIP_VARIANTS silences it as a recorded skip instead.
v2op = {"a": ("GET /A", "T"), "b": ("GET /B", "T")}
perf = {"a": row(1, 2, 3, 1, 2, 3), "b": {**row(1, 2, 3, None, None, None)}}
os.environ.pop("SKIP_VARIANTS", None)
skipped, missing = merge.manifest_check(v2op, perf, None, {})
assert missing == ["b[jellyfin]"], missing
os.environ["SKIP_VARIANTS"] = "b"
skipped, missing = merge.manifest_check(v2op, perf, None, {})
assert skipped == ["b"] and missing == [], (skipped, missing)
os.environ.pop("SKIP_VARIANTS", None)

print("merge self-check: all assertions passed")
