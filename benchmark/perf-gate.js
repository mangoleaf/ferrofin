// Perf regression gate — closed-model, Hermit-only, one endpoint per run.
//
// Reuses bench-lib's auth + ENDPOINTS machinery. Unlike phase-a.js (open-model
// release profiler) this drives a fixed pool of VUs as-fast-as-possible for a
// short window, so a 10 VUs × 10 s run yields thousands of samples → a stable
// p99, and records the 200-rate so the gate can fail on any non-200. Emits the
// same p50/p95/p99 JSON shape phase-a.js does. Add-alongside; phase-a.js is
// unchanged (see plan 04).
import http from 'k6/http';
import { Trend, Counter } from 'k6/metrics';
import { ENDPOINTS, tokenHeaders, authenticate, enrichContext } from './bench-lib.js';

const TARGET = __ENV.TARGET;
const BASE = __ENV.BASE_URL;
const NAME = __ENV.ENDPOINT;
const VUS = parseInt(__ENV.PERF_GATE_VUS || '10', 10);
const SECS = parseInt(__ENV.PERF_GATE_SECONDS || '10', 10);
const WARMUP = __ENV.PERF_GATE_WARMUP || '2s';   // discarded warm-up (JIT/cache fairness)

const ep = ENDPOINTS.find((e) => e.name === NAME);   // checked in fire(), not at init

export const options = {
  summaryTrendStats: ['med', 'p(95)', 'p(99)', 'count'],
  scenarios: {
    // Warm-up VUs (tagged so handleSummary ignores them), then the measured
    // window at the same VU count, back to back.
    warmup: {
      executor: 'constant-vus', vus: VUS, duration: WARMUP,
      exec: 'warm', tags: { phase: 'warmup' },
    },
    measure: {
      executor: 'constant-vus', vus: VUS, duration: `${SECS}s`,
      startTime: WARMUP, exec: 'hit', tags: { phase: 'measure' },
    },
  },
};

// Only the measured scenario feeds these metrics.
const lat = new Trend('lat', true);
const ok = new Counter('ok');
const bad = new Counter('bad');

export function setup() {
  const ctx = authenticate(BASE, TARGET);
  // Same deterministic item pick as phase-a.js: first movie by SortName.
  const items = http.get(
    `${BASE}/Items?userId=${ctx.userId}&Recursive=true&IncludeItemTypes=Movie&SortBy=SortName&Limit=200`,
    tokenHeaders(ctx.token),
  ).json();
  const list = items.Items || [];
  ctx.itemId = list[0] ? list[0].Id : '';
  const withImage = list.find((i) => i.ImageTags && i.ImageTags.Primary);
  ctx.imageItemId = withImage ? withImage.Id : ctx.itemId;
  enrichContext(BASE, ctx);
  return ctx;
}

function fire(ctx) {
  if (!ep) throw new Error(`unknown endpoint: ${NAME}`);
  return http.get(`${BASE}${ep.path(ctx)}`, ep.auth === false ? {} : tokenHeaders(ctx.token));
}
export function warm(ctx) { fire(ctx); }          // warm-up: not recorded
export function hit(ctx) {
  const r = fire(ctx);
  if (r.status === 200) { lat.add(r.timings.duration); ok.add(1); } else { bad.add(1); }
}

export function handleSummary(data) {
  const v = (data.metrics.lat || {}).values || {};
  const okc = (data.metrics.ok || {}).values || {};
  const badc = (data.metrics.bad || {}).values || {};
  const out = {
    target: TARGET, endpoint: NAME, vus: VUS, secs: SECS,
    p50: v.med ?? null, p95: v['p(95)'] ?? null, p99: v['p(99)'] ?? null,
    ok: okc.count || 0, bad: badc.count || 0,
  };
  const key = __ENV.PHASE_OUT || `perfgate-${TARGET}-${NAME}`;
  return { [`results/raw/${key}.json`]: JSON.stringify(out), stdout: '' };
}
