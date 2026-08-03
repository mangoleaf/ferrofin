// k6 driver for the DB pool-size sweep (pool-sweep.sh). Same 50-VU mixed
// lockstep load as scenario.js, but against an ALREADY provisioned + scanned
// Hermit (the sweep scans once and reuses the volume across pool sizes), so
// setup() only authenticates, picks the deterministic item, and warms.
//
//   POOL=8 BASE_URL=http://localhost:18296 k6 run pool-sweep.js
import http from 'k6/http';
import { Trend, Rate } from 'k6/metrics';
import { ENDPOINTS, tokenHeaders, authenticate } from './bench-lib.js';

const BASE = __ENV.BASE_URL;
const POOL = __ENV.POOL || '?';

export const options = {
  setupTimeout: '5m',
  summaryTrendStats: ['med', 'p(95)', 'p(99)', 'count'],
  scenarios: {
    load: { executor: 'constant-vus', vus: parseInt(__ENV.BENCH_VUS || '50', 10), duration: __ENV.BENCH_DURATION || '30s' },
  },
};

const trends = {};
for (const e of ENDPOINTS) trends[e.name] = new Trend(`lat_${e.name}`, true);
const okRates = {};
for (const e of ENDPOINTS) okRates[e.name] = new Rate(`ok_${e.name}`);

export function setup() {
  const ctx = authenticate(BASE, 'hermit');
  // Same deterministic item pick as scenario.js.
  const items = http.get(`${BASE}/Items?userId=${ctx.userId}&Recursive=true&IncludeItemTypes=Movie&SortBy=SortName&Limit=200`, tokenHeaders(ctx.token)).json();
  const list = (items.Items || []);
  if (!list.length) throw new Error('library is empty — pool-sweep.sh must scan before sweeping');
  ctx.itemId = list[0].Id;
  const withImage = list.find((i) => i.ImageTags && i.ImageTags.Primary);
  ctx.imageItemId = withImage ? withImage.Id : ctx.itemId;

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

export function handleSummary(data) {
  const durSec = parseInt((__ENV.BENCH_DURATION || '30s').replace('s', ''), 10) || 30;
  const out = { pool: POOL, durationSec: durSec, endpoints: {} };
  for (const e of ENDPOINTS) {
    const v = (data.metrics[`lat_${e.name}`] || {}).values;
    const ok = (data.metrics[`ok_${e.name}`] || {}).values;
    out.endpoints[e.name] = v ? {
      p50: +v.med.toFixed(2), p95: +v['p(95)'].toFixed(2), p99: +v['p(99)'].toFixed(2),
      count: v.count, rps: +(v.count / durSec).toFixed(1),
      okPct: ok ? +(ok.rate * 100).toFixed(1) : 0,
    } : { p50: null, p95: null, p99: null, count: 0, rps: 0, okPct: 0 };
  }
  const path = `results/raw/pool-${POOL}-summary.json`;
  return { [path]: JSON.stringify(out, null, 2), stdout: `\npool=${POOL} done\n` };
}
