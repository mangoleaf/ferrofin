// Phase A — isolated, open-model, one endpoint at a time.
//
// Why this shape (see the research write-up in RESEARCH/benchmark-methodology):
//   * ISOLATED: only this one endpoint is driven, so its p50/95/99 and the
//     server CPU it burns are attributable to that handler, not to interference
//     from the other 82 endpoints.
//   * OPEN MODEL: a constant *arrival rate* (constant-arrival-rate executor),
//     not a closed VU loop. Requests are dispatched on a fixed schedule
//     regardless of how fast the server answers, so a stall inflates the tail of
//     every request that *should* have gone out — avoiding coordinated omission
//     (Gil Tene) that a think-time-free closed loop hides.
//
// The orchestrator (run-phase-a.sh) brings the server up + scans ONCE, then runs
// this per endpoint, snapshotting the container's cgroup cpu.stat before/after so
// it can attribute CPU-seconds-per-request. This script only measures latency.
import http from 'k6/http';
import { Trend, Counter } from 'k6/metrics';
import { ENDPOINTS, tokenHeaders, authenticate } from './bench-lib.js';

const TARGET = __ENV.TARGET;
const BASE = __ENV.BASE_URL;
const NAME = __ENV.ENDPOINT;
const RATE = parseInt(__ENV.PHASE_RATE || '50', 10);        // requests/sec (open model)
const DUR = __ENV.PHASE_DUR || '20s';                       // measured window
const WARMUP = __ENV.PHASE_WARMUP || '5s';                  // discarded warm-up
const PRE_VUS = parseInt(__ENV.PHASE_PRE_VUS || '30', 10);
const MAX_VUS = parseInt(__ENV.PHASE_MAX_VUS || '200', 10);

const ep = ENDPOINTS.find((e) => e.name === NAME);   // checked in fire(), not at init

export const options = {
  summaryTrendStats: ['med', 'p(95)', 'p(99)', 'count'],
  scenarios: {
    // Warm-up arrivals (tagged so handleSummary can ignore them), then the
    // measured window at the same rate. Two scenarios, back to back.
    warmup: {
      executor: 'constant-arrival-rate', rate: RATE, timeUnit: '1s',
      duration: WARMUP, preAllocatedVUs: PRE_VUS, maxVUs: MAX_VUS,
      exec: 'warm', tags: { phase: 'warmup' },
    },
    measure: {
      executor: 'constant-arrival-rate', rate: RATE, timeUnit: '1s',
      duration: DUR, preAllocatedVUs: PRE_VUS, maxVUs: MAX_VUS,
      startTime: WARMUP, exec: 'hit', tags: { phase: 'measure' },
    },
  },
};

// Only the measured scenario feeds these metrics.
const lat = new Trend('lat', true);
const ok = new Counter('ok');

export function setup() {
  const ctx = authenticate(BASE, TARGET);
  // Same deterministic item pick as the load bench: first movie by SortName.
  const items = http.get(
    `${BASE}/Items?userId=${ctx.userId}&Recursive=true&IncludeItemTypes=Movie&SortBy=SortName&Limit=200`,
    tokenHeaders(ctx.token),
  ).json();
  const list = items.Items || [];
  ctx.itemId = list[0] ? list[0].Id : '';
  const withImage = list.find((i) => i.ImageTags && i.ImageTags.Primary);
  ctx.imageItemId = withImage ? withImage.Id : ctx.itemId;
  return ctx;
}

function fire(ctx) {
  if (!ep) throw new Error(`unknown endpoint: ${NAME}`);
  return http.get(`${BASE}${ep.path(ctx)}`, ep.auth === false ? {} : tokenHeaders(ctx.token));
}
export function warm(ctx) { fire(ctx); }          // warm-up: not recorded
export function hit(ctx) {
  const r = fire(ctx);
  if (r.status === 200) { lat.add(r.timings.duration); ok.add(1); }
}

export function handleSummary(data) {
  const v = (data.metrics.lat || {}).values || {};
  const okc = (data.metrics.ok || {}).values || {};
  const dropped = (data.metrics.dropped_iterations || {}).values || {};
  const reqs = (data.metrics.http_reqs || {}).values || {};
  const out = {
    target: TARGET, endpoint: NAME, rate: RATE, dur: DUR,
    p50: v.med ?? null, p95: v['p(95)'] ?? null, p99: v['p(99)'] ?? null,
    count: okc.count || 0, dropped: dropped.count || 0,
    // total requests over warmup+measure — the denominator for CPU-per-request,
    // since the orchestrator snapshots cgroup cpu.stat around the whole k6 run.
    reqs: reqs.count || 0,
  };
  return { [`results/raw/phaseA-${TARGET}-${NAME}.json`]: JSON.stringify(out), stdout: '' };
}
