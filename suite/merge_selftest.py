#!/usr/bin/env python3
"""Self-check for merge.py's pure comparison logic (bats-invoked, no data files).

Covers D1's tie semantics — the noise floor makes sub-floor deltas neither win
nor loss — and the A1 manifest checker's missing/skip accounting.
"""
import os

# Resolved at config-import time: keep the cold/TTFS expectations out of this
# pure-logic check (the bats fail-loud tests cover those manifest legs).
os.environ["BENCH_COLD_ENDPOINTS"] = ""
os.environ["RUN_TRANSCODE"] = "0"

import merge  # noqa: E402


def row(h50, h95, h99, j50, j95, j99):
    return {"h_p50": h50, "h_p95": h95, "h_p99": h99,
            "j_p50": j50, "j_p95": j95, "j_p99": j99}


FLOOR = 3.0

# Clear win on all three (every delta ≥ floor).
assert merge.percentile_verdicts(row(10, 20, 30, 20, 40, 60), FLOOR) == \
    {"p50": "win", "p95": "win", "p99": "win"}
# Sub-floor deltas are ties on every percentile — the k6-era "5 ms artifact"
# class: a 1-2 ms difference on a sub-ms endpoint is jitter, not a result.
assert merge.percentile_verdicts(row(1.0, 2.0, 3.0, 2.0, 3.5, 5.0), FLOOR) == \
    {"p50": "tie", "p95": "tie", "p99": "tie"}
# p50 win, p99 loss (both clearing the floor) — the tail-loss shape.
v = merge.percentile_verdicts(row(10, 20, 100, 20, 21, 50), FLOOR)
assert v == {"p50": "win", "p95": "tie", "p99": "loss"}
# Exactly-floor delta is NOT a tie (strictly-less-than floor ties).
assert merge.percentile_verdicts(row(10, 10, 10, 13, 13, 13), FLOOR)["p50"] == "win"
# Missing numbers on either side → None (row can't be judged).
assert merge.percentile_verdicts(row(None, 20, 30, 20, 40, 60), FLOOR) is None

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
