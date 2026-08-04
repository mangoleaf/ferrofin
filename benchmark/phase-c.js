// Phase C — the MIXED contention run. All endpoints hit concurrently in a closed
// VU loop (like the original load bench) — NOT for per-endpoint numbers (Phase A
// gives those), but to expose cross-endpoint interference that isolation hides:
// the shared DB pool, locks, caches. setup() only auths (bootstrap.js already
// scanned), so run-phase-c.sh can read cgroup memory.peak around the load window.
import http from 'k6/http';
import { Trend, Rate } from 'k6/metrics';
import { ENDPOINTS, fire, okStatus, tokenHeaders, authenticate } from './bench-lib.js';

const TARGET = __ENV.TARGET;
const BASE = __ENV.BASE_URL;

export const options = {
  summaryTrendStats: ['med', 'p(95)', 'p(99)', 'count'],
  scenarios: {
    mixed: {
      executor: 'constant-vus',
      vus: parseInt(__ENV.BENCH_VUS || '50', 10),
      duration: __ENV.BENCH_DURATION || '30s',
    },
  },
};

const trends = {}, oks = {};
for (const e of ENDPOINTS) { trends[e.name] = new Trend(`lat_${e.name}`, true); oks[e.name] = new Rate(`ok_${e.name}`); }

export function setup() {
  const ctx = authenticate(BASE, TARGET);
  const items = http.get(
    `${BASE}/Items?userId=${ctx.userId}&Recursive=true&IncludeItemTypes=Movie&SortBy=SortName&Limit=200`,
    tokenHeaders(ctx.token),
  ).json();
  const list = items.Items || [];
  ctx.itemId = list[0] ? list[0].Id : '';
  const withImage = list.find((i) => i.ImageTags && i.ImageTags.Primary);
  ctx.imageItemId = withImage ? withImage.Id : ctx.itemId;
  // Write rows' target (phase-c skips enrichContext): same last-movie pick as there.
  ctx.writeItemId = list.length ? list[list.length - 1].Id : ctx.itemId;
  return ctx;
}

export default function (ctx) {
  for (const e of ENDPOINTS) {
    if (e.scenario) continue;                     // own-window rows (auth_login) skip the mixed loop
    const r = fire(BASE, e, ctx);
    oks[e.name].add(r.status === okStatus(e));
    if (r.status === okStatus(e)) trends[e.name].add(r.timings.duration);
  }
}

export function handleSummary(data) {
  const out = { target: TARGET, endpoints: {} };
  for (const e of ENDPOINTS) {
    const v = (data.metrics[`lat_${e.name}`] || {}).values;
    const ok = (data.metrics[`ok_${e.name}`] || {}).values;
    out.endpoints[e.name] = v
      ? { p50: +v.med.toFixed(2), p95: +v['p(95)'].toFixed(2), p99: +v['p(99)'].toFixed(2), count: v.count, okPct: ok ? +(ok.rate * 100).toFixed(1) : 0 }
      : { p50: null, p95: null, p99: null, count: 0, okPct: 0 };
  }
  return { [`results/raw/phaseC-${TARGET}.json`]: JSON.stringify(out), stdout: '' };
}
