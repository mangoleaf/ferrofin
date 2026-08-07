// k6 driver. Same script hits either server; TARGET picks the provisioning path.
// setup() runs once: create library -> scan -> wait until item count is stable -> warm.
// The VU loop then hits each read endpoint, recording latency into a per-endpoint Trend.
//
// Run indirectly via run.sh. Direct:
//   TARGET=hermit BASE_URL=http://localhost:8096 k6 run scenario.js
import http from 'k6/http';
import { Trend, Rate } from 'k6/metrics';
import { ENDPOINTS, fire, okStatus, tokenHeaders, bringUp, enrichContext } from './bench-lib.js';

const TARGET = __ENV.TARGET;                 // 'hermit' | 'jellyfin'
const BASE = __ENV.BASE_URL;
const EXPECTED = parseInt(__ENV.EXPECTED_ITEMS || '0', 10);
const MAIN_SECS = parseInt((__ENV.BENCH_DURATION || '30s').replace('s', ''), 10) || 30;
const LOGIN_SECS = parseInt((__ENV.BENCH_LOGIN_DURATION || '15s').replace('s', ''), 10) || 15;

export const options = {
  setupTimeout: '45m',   // setup() waits for the full library scan; default 60s would abort the run
  summaryTrendStats: ['med', 'p(95)', 'p(99)', 'count'],
  scenarios: {
    load: { executor: 'constant-vus', vus: parseInt(__ENV.BENCH_VUS || '50', 10), duration: __ENV.BENCH_DURATION || '30s' },
    // The login storm (auth_login) gets its OWN window after `load` fully drains
    // (+30s = load's default gracefulStop): PBKDF2 saturates CPU and every login
    // invalidates the server-side auth cache, so in-loop it would poison every other
    // row. Fewer VUs than `load` — 50 concurrent PBKDF2s on 4 cores would measure
    // pure CPU queueing, not the login path.
    login: {
      executor: 'constant-vus', exec: 'login',
      vus: parseInt(__ENV.BENCH_LOGIN_VUS || '10', 10),
      duration: `${LOGIN_SECS}s`, startTime: `${MAIN_SECS + 30}s`,
    },
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
  // Hand the provisioned, pre-throttle token to run.sh's post-load captures (count,
  // fingerprint, transcode) — it greps this line from k6's output. A fresh post-load
  // login would 500 against a throttled Jellyfin, and on Jellyfin the bench user only
  // exists after the wizard above, so there's no earlier moment to mint it. setup() state
  // doesn't reach handleSummary() in k6 (separate runtime), hence the console channel.
  console.log(`CAPTURE_CREDS ${ctx.token} ${ctx.userId}`);

  // Deterministic item pick: first movie by SortName on BOTH servers (ids differ, item is the
  // same). For the image row, prefer the first of those with a discovered Primary image —
  // otherwise a poster-less pick turns the row into a 404 microbenchmark.
  const items = http.get(`${BASE}/Items?userId=${ctx.userId}&Recursive=true&IncludeItemTypes=Movie&SortBy=SortName&Limit=200`, tokenHeaders(ctx.token)).json();
  const list = (items.Items || []);
  ctx.itemId = list[0] ? list[0].Id : '';
  const withImage = list.find((i) => i.ImageTags && i.ImageTags.Primary);
  ctx.imageItemId = withImage ? withImage.Id : ctx.itemId;
  enrichContext(BASE, ctx);

  // Warm: repeated passes so we measure steady-state, not cold caches — and, for Jellyfin,
  // so .NET tiered-JIT recompilation happens before the window, not inside it.
  const warmUntil = Date.now() + (parseInt(__ENV.BENCH_WARMUP_SECONDS || '10', 10) * 1000);
  while (Date.now() < warmUntil)
    for (const e of ENDPOINTS) { if (!e.scenario) fire(BASE, e, ctx); }
  return ctx;
}

export default function (ctx) {
  for (const e of ENDPOINTS) {
    if (e.scenario) continue;                       // own-window rows (auth_login) skip the mixed loop
    const r = fire(BASE, e, ctx);
    okRates[e.name].add(r.status === okStatus(e));
    if (r.status === okStatus(e)) trends[e.name].add(r.timings.duration);
  }
}

// The login-storm window: one full login per iteration (PBKDF2 verify + token mint
// + device upsert). Latency recorded under the same 200-only fairness rule.
export function login(ctx) {
  const e = ENDPOINTS.find((x) => x.name === 'auth_login');
  const r = fire(BASE, e, ctx);
  okRates[e.name].add(r.status === okStatus(e));
  if (r.status === okStatus(e)) trends[e.name].add(r.timings.duration);
}

// Emit one machine-readable summary per run; run.sh merges the two into the report.
export function handleSummary(data) {
  const durSec = parseInt((__ENV.BENCH_DURATION || '30s').replace('s', ''), 10) || 30;
  const out = { target: TARGET, durationSec: durSec, endpoints: {} };
  for (const e of ENDPOINTS) {
    const v = (data.metrics[`lat_${e.name}`] || {}).values;   // absent if the endpoint never returned its ok status
    const ok = (data.metrics[`ok_${e.name}`] || {}).values;
    const dur = e.scenario === 'login' ? LOGIN_SECS : durSec;  // rps over the row's own window
    out.endpoints[e.name] = v ? {
      p50: +v.med.toFixed(2), p95: +v['p(95)'].toFixed(2), p99: +v['p(99)'].toFixed(2),
      count: v.count, rps: +(v.count / dur).toFixed(1),
      okPct: ok ? +(ok.rate * 100).toFixed(1) : 0,
    } : { p50: null, p95: null, p99: null, count: 0, rps: 0, okPct: 0 };
  }
  const path = `results/raw/${TARGET}-summary.json`;
  return { [path]: JSON.stringify(out, null, 2), stdout: `\n${TARGET}: ${JSON.stringify(out.endpoints)}\n` };
}
