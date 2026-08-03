#!/usr/bin/env python3
"""Self-check for gate.py's classify() — ported verbatim from the retired
perf-gate.test.mjs so the consolidation lost no test cases.
Run: python3 gate_selftest.py"""
from gate import classify

BASE = {"p50": 10, "p95": 20, "p99": 30}


def ok(**extra):
    cur = {"p50": 10, "p95": 20, "p99": 30, "ok": 5000, "bad": 0}
    cur.update(extra)
    return cur


# Within factor on every percentile → clean.
assert classify(BASE, ok(p50=14, p95=29, p99=44), 1.5) == [], "under 1.5× everywhere → ok"

# Tail-only regression: p50/p95 fine, p99 3× → caught (median-only gating would miss this).
assert classify(BASE, ok(p99=90), 1.5) == ["p99"], "p99-only regression caught"

# p50 win but p95 tail loss is still a fail (a fast median never excuses a slow tail).
assert classify(BASE, ok(p50=5, p95=40), 1.5) == ["p95"], "p50 win + p95 loss → p95 fail"

# Any non-200 fails the 200-rate check regardless of latency.
assert classify(BASE, ok(bad=1), 1.5) == ["200%"], "one non-200 → 200% fail"

# No data (k6 produced no measured 200s) → cannot verify → fail.
assert classify(BASE, {"ok": 0, "bad": 0}, 1.5) == ["nodata"], "no measured 200s → nodata"
assert classify(BASE, None, 1.5) == ["nodata"], "missing result → nodata"

# Exactly at the factor is NOT a regression (strictly greater trips).
assert classify(BASE, ok(p99=45), 1.5) == [], "p99 == 1.5× → not a regression"

# Zero-baseline percentile with a nonzero current is an infinite ratio → trips.
assert classify({"p50": 0, "p95": 20, "p99": 30}, ok(), 1.5) == ["p50"], "0-baseline + nonzero cur → trips"

print("gate self-check: all assertions passed")
