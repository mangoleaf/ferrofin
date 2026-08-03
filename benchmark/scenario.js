// k6 driver. Same script hits either server; TARGET picks the provisioning path.
// setup() runs once: create library -> scan -> wait until item count is stable -> warm.
// The VU loop then hits each read endpoint, recording latency into a per-endpoint Trend.
//
// Run indirectly via run.sh. Direct:
//   TARGET=hermit BASE_URL=http://localhost:8096 k6 run scenario.js
import http from 'k6/http';
import { Trend, Rate } from 'k6/metrics';
import { ENDPOINTS, tokenHeaders, bringUp } from './bench-lib.js';

const TARGET = __ENV.TARGET;                 // 'hermit' | 'jellyfin'
const BASE = __ENV.BASE_URL;
const EXPECTED = parseInt(__ENV.EXPECTED_ITEMS || '0', 10);

export const options = {
  setupTimeout: '45m',   // setup() waits for the full library scan; default 60s would abort the run
  summaryTrendStats: ['med', 'p(95)', 'p(99)', 'count'],
  scenarios: {
    load: { executor: 'constant-vus', vus: parseInt(__ENV.BENCH_VUS || '50', 10), duration: __ENV.BENCH_DURATION || '30s' },
  },
};

// ENDPOINTS (the read surface) is defined in bench-lib.js; suite/registry.json mirrors it by op.
const trends = {};
for (const e of ENDPOINTS) trends[e.name] = new Trend(`lat_${e.name}`, true);
// Fairness: only 200s enter the latency trend (an error path is cheap and would fake a win);
// non-200s are surfaced as okRate in the summary so a broken row is flagged, not hidden.
const okRates = {};
for (const e of ENDPOINTS) okRates[e.name] = new Rate(`ok_${e.name}`);

export function setup() {
  const ctx = bringUp(BASE, TARGET, EXPECTED);   // wizard(jellyfin) -> auth -> provision -> wait scan

  // Deterministic item pick: first movie by SortName on BOTH servers (ids differ, item is the
  // same). For the image row, prefer the first of those with a discovered Primary image —
  // otherwise a poster-less pick turns the row into a 404 microbenchmark.
  const items = http.get(`${BASE}/Items?userId=${ctx.userId}&Recursive=true&IncludeItemTypes=Movie&SortBy=SortName&Limit=200`, tokenHeaders(ctx.token)).json();
  const list = (items.Items || []);
  ctx.itemId = list[0] ? list[0].Id : '';
  const withImage = list.find((i) => i.ImageTags && i.ImageTags.Primary);
  ctx.imageItemId = withImage ? withImage.Id : ctx.itemId;

  // Warm: repeated passes so we measure steady-state, not cold caches — and, for Jellyfin,
  // so .NET tiered-JIT recompilation happens before the window, not inside it.
  const warmUntil = Date.now() + (parseInt(__ENV.BENCH_WARMUP_SECONDS || '10', 10) * 1000);
  while (Date.now() < warmUntil)
    for (const e of ENDPOINTS) http.get(`${BASE}${e.path(ctx)}`, e.auth === false ? {} : tokenHeaders(ctx.token));
  return ctx;
}

export default function (ctx) {
  for (const e of ENDPOINTS) {
    const r = http.get(`${BASE}${e.path(ctx)}`, e.auth === false ? {} : tokenHeaders(ctx.token));
    okRates[e.name].add(r.status === 200);
    if (r.status === 200) trends[e.name].add(r.timings.duration);
  }
}

// Emit one machine-readable summary per run; run.sh merges the two into the report.
export function handleSummary(data) {
  const durSec = parseInt((__ENV.BENCH_DURATION || '30s').replace('s', ''), 10) || 30;
  const out = { target: TARGET, durationSec: durSec, endpoints: {} };
  for (const e of ENDPOINTS) {
    const v = (data.metrics[`lat_${e.name}`] || {}).values;   // absent if the endpoint never returned 200
    const ok = (data.metrics[`ok_${e.name}`] || {}).values;
    out.endpoints[e.name] = v ? {
      p50: +v.med.toFixed(2), p95: +v['p(95)'].toFixed(2), p99: +v['p(99)'].toFixed(2),
      count: v.count, rps: +(v.count / durSec).toFixed(1),
      okPct: ok ? +(ok.rate * 100).toFixed(1) : 0,
    } : { p50: null, p95: null, p99: null, count: 0, rps: 0, okPct: 0 };
  }
  const path = `results/raw/${TARGET}-summary.json`;
  return { [path]: JSON.stringify(out, null, 2), stdout: `\n${TARGET}: ${JSON.stringify(out.endpoints)}\n` };
}
