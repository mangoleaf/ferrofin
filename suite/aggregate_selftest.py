#!/usr/bin/env python3
"""Self-check for aggregate.py's pure reduction logic (bats-invoked, no files).

Pins the two rules the published headline hangs on: paired ratios are ratios
of MEDIANS with the noise floor applied (sub-floor pairs carry no ratio and
are counted), and per-endpoint distributions are median ± IQR across runs.
"""
import os

os.environ["BENCH_COLD_ENDPOINTS"] = ""
os.environ["BENCH_NOISE_FLOOR_MS"] = "3"

import aggregate  # noqa: E402

# ── paired_speedup_of: the ratio rule ────────────────────────────────────────
# Clear win: 100 ms vs 300 ms → 3.0×.
assert aggregate.paired_speedup_of(100, 300, 3.0) == (3.0, False)
# Sub-floor pair: 0.1 vs 0.3 ms would read "3×" — it is a TIE, no ratio.
assert aggregate.paired_speedup_of(0.1, 0.3, 3.0) == (None, True)
# Exactly-floor delta is NOT a tie (strictly-less-than ties, matching merge).
assert aggregate.paired_speedup_of(10, 13, 3.0) == (1.3, False)
# Missing either side: no ratio, not a tie.
assert aggregate.paired_speedup_of(None, 300, 3.0) == (None, False)
assert aggregate.paired_speedup_of(100, None, 3.0) == (None, False)
# Zero medians classify too (round 3): sub-floor counterpart is a tie,
# floor-clearing counterpart is an undefined ratio (never silent AND uncounted
# when both sides were actually measured close together).
assert aggregate.paired_speedup_of(0, 1, 3.0) == (None, True)
assert aggregate.paired_speedup_of(0, 300, 3.0) == (None, False)

# ── dist: median ± IQR across runs ───────────────────────────────────────────
d = aggregate.dist([10, 20, 30, 40])
assert d["med"] == 25 and d["n"] == 4 and d["min"] == 10 and d["max"] == 40
assert aggregate.dist([7]) == {"med": 7, "iqr": 0.0, "min": 7, "max": 7, "n": 1}
assert aggregate.dist([None, None]) is None

# ── aggregate(): end-to-end over synthetic same-SHA runs ─────────────────────
def run(h50_fast, j50_fast, h50_tie, j50_tie):
    def op(variant, h50, j50, comparable=True):
        return {"op": f"GET /{variant}", "owner": "core",
                "parity": {"deep_verified": True},
                "perf": {"variant": variant, "comparable": comparable,
                         "f_p50": h50, "f_p95": h50 * 2, "f_p99": h50 * 3,
                         "j_p50": j50, "j_p95": j50 * 2, "j_p99": j50 * 3,
                         # A cold block on one op: cold aggregation (f/j keys) must
                         # reduce AND render — a rename that misses a cold consumer
                         # only crashes on cold-bearing data (round-2 regression).
                         **({"cold": {"f_first": h50 * 5, "j_first": j50 * 5}}
                            if variant == "fast" else {})}}
    return {"meta": {"ferrofin_sha": "abc", "ferrofin": "vX",
                     "load": {"model": "open-loop"}},
            "headline": {"parity_coverage": 1.0, "comparable_rows": 2,
                         "win_rate": 1.0, "ties": 0, "median_speedup": None},
            "operations": [op("fast", h50_fast, j50_fast),
                           op("tiny", h50_tie, j50_tie)]}


agg = aggregate.aggregate([run(100, 300, 0.1, 0.3), run(110, 310, 0.2, 0.4)])
fast, tiny = agg["endpoints"]["fast"], agg["endpoints"]["tiny"]
# fast: medians 105 vs 305 → paired ratio ~2.9×, IQR present.
assert fast["f_p50"]["med"] == 105 and fast["j_p50"]["med"] == 305
assert fast["paired_speedup"] == 2.9, fast["paired_speedup"]
# tiny: sub-floor medians → NO ratio, counted as a paired tie.
assert tiny["paired_speedup"] is None and tiny.get("paired_tie") is True
assert agg["headline"]["paired_excluded_ties"] == 1
# The headline paired distribution covers only the ratio-carrying endpoint.
assert agg["headline"]["paired_speedup"]["n"] == 1
assert agg["headline"]["noise_floor_ms"] == 3.0
# Cold firsts reduce under the f/j keys and the markdown renderer accepts them.
assert fast["cold_first"]["f"]["med"] == 525 and fast["cold_first"]["j"]["med"] == 1525
assert "525" in aggregate.render_md(agg, "abc")

print("aggregate self-check: all assertions passed")
