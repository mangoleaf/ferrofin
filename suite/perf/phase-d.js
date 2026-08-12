// Phase D — the "what users feel" load: a handful of virtual clients behaving
// like real ones, instead of 50 zero-think-time VUs hammering in lockstep.
//
// Each VU is one client app on its own device (own login + DeviceId — reusing
// one DeviceId across VUs makes the servers fold every reporter into a single
// session). A session iteration: home screen → browse a library page → open an
// item's detail → fetch posters → start playback (PlaybackInfo + playstate
// start/progress/stop, exercising the write path) — with 1–3 s think time
// between steps.
//
// Run via run-phase-d.sh (which boots + scans the server first). Direct:
//   TARGET=ferrofin BASE_URL=http://localhost:18196 k6 run phase-d.js
import http from 'k6/http';
import { sleep } from 'k6';
import exec from 'k6/execution';
import { Trend, Rate } from 'k6/metrics';
import { tokenHeaders, authenticate } from './bench-lib.js';

const TARGET = __ENV.TARGET;
const BASE = __ENV.BASE_URL;
const USER = __ENV.BENCH_ADMIN_USER || 'bench';
const PASS = __ENV.BENCH_ADMIN_PASSWORD || 'benchpass123';

export const options = {
  setupTimeout: '5m',
  summaryTrendStats: ['med', 'p(95)', 'p(99)', 'count'],
  scenarios: {
    clients: {
      executor: 'constant-vus',
      vus: parseInt(__ENV.PHASE_D_VUS || '8', 10),
      duration: __ENV.PHASE_D_DUR || '120s',
      gracefulStop: '10s',
    },
  },
};

// One latency trend per journey step; every request in a step feeds it.
const STEPS = ['home', 'library', 'detail', 'images', 'playback'];
const trends = Object.fromEntries(STEPS.map((s) => [s, new Trend(`step_${s}`, true)]));
const okRate = new Rate('ok_all');
const sessions = new Trend('session_ms', true);

const think = () => sleep(1 + Math.random() * 2);

export function setup() {
  const ctx = authenticate(BASE, TARGET);
  const items = http.get(`${BASE}/Items?userId=${ctx.userId}&Recursive=true&IncludeItemTypes=Movie&SortBy=SortName&Limit=200`, tokenHeaders(ctx.token)).json();
  const list = items.Items || [];
  if (!list.length) throw new Error('library is empty — run bootstrap.js first');
  return {
    itemIds: list.map((i) => i.Id),
    imageIds: list.filter((i) => i.ImageTags && i.ImageTags.Primary).map((i) => i.Id).slice(0, 24),
  };
}

// Per-VU client identity, established on the VU's first iteration.
let me = null;
function login() {
  const vu = exec.vu.idInTest;
  const clientId = `Client="bench-d", Device="phone-${vu}", DeviceId="phase-d-vu${vu}", Version="1.0"`;
  const r = http.post(`${BASE}/Users/AuthenticateByName`,
    JSON.stringify({ Username: USER, Pw: PASS }),
    { headers: { 'Content-Type': 'application/json', Authorization: `MediaBrowser ${clientId}` } });
  if (r.status !== 200) throw new Error(`[${TARGET}] vu${vu} auth failed: ${r.status}`);
  const b = r.json();
  return {
    userId: b.User.Id,
    headers: { headers: { Authorization: `MediaBrowser Token="${b.AccessToken}", ${clientId}`, 'Content-Type': 'application/json' } },
  };
}

function step(name, requests) {
  for (const [method, url, body] of requests) {
    const r = method === 'GET' ? http.get(url, me.headers) : http.post(url, body ? JSON.stringify(body) : null, me.headers);
    okRate.add(r.status < 400);
    if (r.status < 400) trends[name].add(r.timings.duration);
  }
}

export default function (data) {
  if (!me) me = login();
  const start = Date.now();
  const uid = me.userId;
  const pick = (arr) => arr[Math.floor(Math.random() * arr.length)];
  const itemId = pick(data.itemIds);

  step('home', [
    ['GET', `${BASE}/UserViews?userId=${uid}`],
    ['GET', `${BASE}/Items/Latest?userId=${uid}&limit=20`],
    ['GET', `${BASE}/UserItems/Resume?userId=${uid}&limit=12`],
    ['GET', `${BASE}/Shows/NextUp?userId=${uid}&limit=24`],
  ]);
  think();

  const page = Math.floor(Math.random() * 4) * 50;
  step('library', [
    ['GET', `${BASE}/Items?userId=${uid}&recursive=true&includeItemTypes=Movie&limit=50&startIndex=${page}&sortBy=SortName&fields=PrimaryImageAspectRatio,MediaSourceCount`],
    ['GET', `${BASE}/Genres?userId=${uid}`],
  ]);
  think();

  step('detail', [
    ['GET', `${BASE}/Items/${itemId}?userId=${uid}`],
    ['GET', `${BASE}/Items/${itemId}/Similar?userId=${uid}&limit=12`],
  ]);
  step('images', data.imageIds.slice(0, 3).map((id) => (
    ['GET', `${BASE}/Items/${id}/Images/Primary?maxWidth=400&quality=90`]
  )));
  think();

  // Playback: resolve sources, then the playstate write path a real client
  // drives (start → progress → stop). PositionTicks in 100 ns ticks.
  step('playback', [
    ['GET', `${BASE}/Items/${itemId}/PlaybackInfo?userId=${uid}`],
    ['POST', `${BASE}/Sessions/Playing`, { ItemId: itemId, PositionTicks: 0, CanSeek: true, PlayMethod: 'DirectPlay' }],
  ]);
  sleep(1 + Math.random());
  step('playback', [
    ['POST', `${BASE}/Sessions/Playing/Progress`, { ItemId: itemId, PositionTicks: 600_000_000, PlayMethod: 'DirectPlay' }],
    ['POST', `${BASE}/Sessions/Playing/Stopped`, { ItemId: itemId, PositionTicks: 1_200_000_000 }],
  ]);

  sessions.add(Date.now() - start);
}

export function handleSummary(data) {
  const out = { target: TARGET, steps: {}, sessions: null, okPct: null };
  for (const s of STEPS) {
    const v = (data.metrics[`step_${s}`] || {}).values;
    out.steps[s] = v ? { p50: +v.med.toFixed(2), p95: +v['p(95)'].toFixed(2), p99: +v['p(99)'].toFixed(2), count: v.count } : null;
  }
  const sess = (data.metrics.session_ms || {}).values;
  out.sessions = sess ? { p50: +sess.med.toFixed(0), p95: +sess['p(95)'].toFixed(0), count: sess.count } : null;
  const ok = (data.metrics.ok_all || {}).values;
  out.okPct = ok ? +(ok.rate * 100).toFixed(1) : null;
  return {
    [`results/raw/phaseD-${TARGET}.json`]: JSON.stringify(out, null, 2),
    stdout: `\n[${TARGET}] phase D: ${JSON.stringify(out.steps)}\n`,
  };
}
