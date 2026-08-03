// Self-check for perf-gate.mjs's classify(). Run: node perf-gate.test.mjs
import assert from 'node:assert/strict';
import { classify } from './perf-gate.mjs';

const base = { p50: 10, p95: 20, p99: 30 };
const ok = (extra) => ({ p50: 10, p95: 20, p99: 30, ok: 5000, bad: 0, ...extra });

// Within factor on every percentile → clean.
assert.deepEqual(classify(base, ok({ p50: 14, p95: 29, p99: 44 }), 1.5), [], 'under 1.5× everywhere → ok');

// Tail-only regression: p50/p95 fine, p99 3× → caught (median-only gating would miss this).
assert.deepEqual(classify(base, ok({ p99: 90 }), 1.5), ['p99'], 'p99-only regression caught');

// p50 win but p95 tail loss is still a fail (a fast median never excuses a slow tail).
assert.deepEqual(classify(base, ok({ p50: 5, p95: 40 }), 1.5), ['p95'], 'p50 win + p95 loss → p95 fail');

// Any non-200 fails the 200-rate check regardless of latency.
assert.deepEqual(classify(base, ok({ bad: 1 }), 1.5), ['200%'], 'one non-200 → 200% fail');

// No data (k6 produced no measured 200s) → cannot verify → fail.
assert.deepEqual(classify(base, { ok: 0, bad: 0 }, 1.5), ['nodata'], 'no measured 200s → nodata');
assert.deepEqual(classify(base, null, 1.5), ['nodata'], 'missing result → nodata');

// Exactly at the factor is NOT a regression (strictly greater trips).
assert.deepEqual(classify(base, ok({ p99: 45 }), 1.5), [], 'p99 == 1.5× → not a regression');

console.log('perf-gate self-check: all assertions passed');
