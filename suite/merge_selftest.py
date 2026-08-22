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

# ---- shape_check: the honesty excluder (Ferrofin-vs-baseline) + informational cross-server ----

F = {"hash": "aaaa", "paths": ["Id", "Name", "Genres"]}
J_SAME = {"hash": "aaaa", "paths": ["Id", "Name", "Genres"]}
J_DIFF = {"hash": "bbbb", "paths": ["Id", "Name", "Genres", "ImageTags.Primary"]}

# Write rows / uncaptured variants are exempt entirely.
assert merge.shape_check(None, J_DIFF, None, False) == (None, None, None)

# First sighting seeds the baseline, never excludes.
r, blk, upd = merge.shape_check(F, J_SAME, None, False)
assert r is None and blk["baseline"] == "new" and upd is F

# Stable shape matching baseline: comparable, no baseline churn.
r, blk, upd = merge.shape_check(F, J_SAME, {"hash": "aaaa", "paths": F["paths"]}, False)
assert r is None and upd is None and blk["matches_jellyfin"] is True

# Cross-server divergence alone NEVER excludes — it is published with a field diff.
r, blk, upd = merge.shape_check(F, J_DIFF, {"hash": "aaaa", "paths": F["paths"]}, False)
assert r is None, "jellyfin divergence must be informational, not an exclusion"
assert blk["matches_jellyfin"] is False
assert blk["diff_vs_jellyfin"] == {"missing": ["ImageTags.Primary"], "extra": []}

# Ferrofin's own shape changing vs baseline excludes (the hollow-body catch)…
base = {"hash": "cccc", "paths": ["Id", "Name", "Genres", "People[].Name"]}
r, blk, upd = merge.shape_check(F, J_SAME, base, False)
assert r and "changed since baseline" in r and upd is None
assert blk["diff_vs_baseline"] == {"missing": ["People[].Name"], "extra": []}

# …until the change is reviewed and acked, which advances the baseline.
r, blk, upd = merge.shape_check(F, J_SAME, base, True)
assert r is None and blk["ack"] is True and upd is F

# Legacy hash-only captures still work — no paths means no diff, same verdicts.
r, blk, upd = merge.shape_check({"hash": "aaaa", "paths": None}, {"hash": "bbbb", "paths": None},
                                {"hash": "aaaa", "paths": None}, False)
assert r is None and blk["matches_jellyfin"] is False and "diff_vs_jellyfin" not in blk

# A failed probe is NO capture: it must never seed the baseline, exclude, or publish.
assert merge.shape_check({"hash": "error:HTTPError", "paths": []}, J_SAME, None, False) == \
    (None, None, None)
# A Jellyfin-side probe error suppresses the cross-server verdict; the row still
# gates on the Ferrofin baseline as usual.
r, blk, upd = merge.shape_check(F, {"hash": "error:HTTPError", "paths": []},
                                {"hash": "aaaa", "paths": F["paths"]}, False)
assert r is None and "matches_jellyfin" not in blk and "j" not in blk

# An oversized one-sided diff is capped with an elided-count tail, not embedded whole.
big = merge.shape_diff(["A"], [f"P{i}" for i in range(merge.DIFF_PATH_CAP + 5)])
assert len(big["missing"]) == merge.DIFF_PATH_CAP + 1
assert big["missing"][-1].startswith("… +")

# run_signature covers measured numbers ONLY: a pre-ack merge and its acked re-merge
# (same raw artifacts, different headline/exclusions) must collapse to one trend entry.
_rec = {"meta": {"footprint": {"rss": 1}}, "headline": {"comparable_rows": 70},
        "operations": [{"perf": {"variant": "v", "f_p50": 1, "j_p50": 2,
                                 "f_p99": 3, "j_p99": 4}}]}
assert merge.run_signature(_rec) == merge.run_signature({**_rec, "headline": {"comparable_rows": 113}})

# fp_entry: per-variant (new) keys win, op-keyed legacy files still resolve.
assert merge.fp_entry({"items_movies": {"hash": "x", "paths": []}}, "items_movies", "GET /Items")["hash"] == "x"
assert merge.fp_entry({"GET /Items": "y"}, "items_movies", "GET /Items") == {"hash": "y", "paths": None}
assert merge.fp_entry({}, "items_movies", "GET /Items") is None

print("merge self-check: all assertions passed")
