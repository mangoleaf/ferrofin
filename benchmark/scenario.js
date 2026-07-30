// k6 driver. Same script hits either server; TARGET picks the provisioning path.
// setup() runs once: create library -> scan -> wait until item count is stable -> warm.
// The VU loop then hits each read endpoint, recording latency into a per-endpoint Trend.
//
// Run indirectly via run.sh. Direct:
//   TARGET=hermit BASE_URL=http://localhost:8096 k6 run scenario.js
import http from 'k6/http';
import { check, sleep } from 'k6';
import { Trend, Rate } from 'k6/metrics';

const TARGET = __ENV.TARGET;                 // 'hermit' | 'jellyfin'
const BASE = __ENV.BASE_URL;
const USER = __ENV.BENCH_ADMIN_USER || 'bench';
const PASS = __ENV.BENCH_ADMIN_PASSWORD || 'benchpass123';
const EXPECTED = parseInt(__ENV.EXPECTED_ITEMS || '0', 10);

export const options = {
  setupTimeout: '45m',   // setup() waits for the full library scan; default 60s would abort the run
  summaryTrendStats: ['med', 'p(95)', 'p(99)', 'count'],
  scenarios: {
    load: { executor: 'constant-vus', vus: parseInt(__ENV.BENCH_VUS || '50', 10), duration: __ENV.BENCH_DURATION || '30s' },
  },
};

// The endpoints we compare. `path` is templated per-VU from setup() data.
// image is best-effort: N/A on a server that doesn't discover local poster.jpg.
const ENDPOINTS = [
  { name: 'info_public', auth: false, path: () => '/System/Info/Public' },
  { name: 'user_views', path: (c) => `/UserViews?userId=${c.userId}` },
  { name: 'items_sortname', path: (c) => `/Items?userId=${c.userId}&Recursive=true&IncludeItemTypes=Movie&Limit=50&SortBy=SortName` },
  { name: 'items_datesort', path: (c) => `/Items?userId=${c.userId}&Recursive=true&IncludeItemTypes=Movie&Limit=50&SortBy=DateCreated&SortOrder=Descending` },
  { name: 'items_episodes', path: (c) => `/Items?userId=${c.userId}&Recursive=true&IncludeItemTypes=Episode&Limit=50&SortBy=SortName` },
  { name: 'item_detail', path: (c) => `/Items/${c.itemId}?userId=${c.userId}` },
  { name: 'image_primary', path: (c) => `/Items/${c.imageItemId}/Images/Primary?fillHeight=400&fillWidth=400` },
];
const trends = {};
for (const e of ENDPOINTS) trends[e.name] = new Trend(`lat_${e.name}`, true);
// Fairness: only 200s enter the latency trend (an error path is cheap and would fake a win);
// non-200s are surfaced as okRate in the summary so a broken row is flagged, not hidden.
const okRates = {};
for (const e of ENDPOINTS) okRates[e.name] = new Rate(`ok_${e.name}`);

// Modern `Authorization: MediaBrowser …` grammar only: 10.11 ships with
// EnableLegacyAuthorization=false, so X-Emby-Token/X-Emby-Authorization are rejected
// by a fresh install of either server.
const CLIENT_ID = 'Client="bench", Device="bench", DeviceId="bench", Version="1.0"';
function tokenHeaders(token) {
  return { headers: { Authorization: `MediaBrowser Token="${token}", ${CLIENT_ID}`, 'Content-Type': 'application/json' } };
}

function authenticate() {
  const r = http.post(`${BASE}/Users/AuthenticateByName`,
    JSON.stringify({ Username: USER, Pw: PASS }),
    { headers: { 'Content-Type': 'application/json', Authorization: `MediaBrowser ${CLIENT_ID}` } });
  check(r, { 'auth 200': (x) => x.status === 200 });
  if (r.status !== 200) throw new Error(`[${TARGET}] auth failed: ${r.status} ${String(r.body).slice(0, 200)}`);
  const b = r.json();
  return { token: b.AccessToken, userId: b.User.Id };
}

function itemCount(ctx) {
  const r = http.get(`${BASE}/Items?userId=${ctx.userId}&Recursive=true&IncludeItemTypes=Movie,Episode&Limit=0`, tokenHeaders(ctx.token));
  return r.status === 200 ? (r.json().TotalRecordCount || 0) : -1;
}

// Poll until the library reaches EXPECTED (or stops growing). Shared, API-defined
// completion signal — works identically on both servers, no scan-status API needed.
function waitForScan(ctx) {
  let last = -1, stable = 0, zeros = 0;
  for (let i = 0; i < 480; i++) {          // ponytail: 480*5s = 40min cap, under the 45m setupTimeout
    const n = itemCount(ctx);
    console.log(`[${TARGET}] scan progress: ${n}/${EXPECTED} items`);
    if (EXPECTED > 0 && n >= EXPECTED) return n;
    stable = (n === last && n > 0) ? stable + 1 : 0;
    if (stable >= 3) return n;             // count held 3 polls => scan done (unknown-total fallback)
    zeros = n <= 0 ? zeros + 1 : 0;
    if (zeros >= 36) throw new Error(`[${TARGET}] still 0 items after 3 minutes — scan never started`);
    last = n;
    sleep(5);
  }
  return last;
}

// Libraries come from run.sh (LIBRARIES env) — real media and/or synthetic padding —
// so the same code provisions whatever you point it at.
function provision(ctx) {
  const h = tokenHeaders(ctx.token);
  // Fairness: Hermit's remote metadata providers are inert (feature-gated / no API keys), so
  // Jellyfin's must be off too — empty LibraryOptions would leave its TMDB/OMDb fetchers on
  // (slower scan, network-dependent, richer DTOs = a different workload). TypeOptions with
  // empty fetcher arrays disables them; local NFO/image providers stay on for both.
  const noRemote = {
    LibraryOptions: {
      EnableRealtimeMonitor: false,
      SaveLocalMetadata: false,
      TypeOptions: ['Movie', 'Series', 'Season', 'Episode'].map((t) => (
        { Type: t, MetadataFetchers: [], MetadataFetcherOrder: [], ImageFetchers: [], ImageFetcherOrder: [] })),
    },
  };
  for (const l of JSON.parse(__ENV.LIBRARIES || '[]')) {
    const q = `name=${encodeURIComponent(l.name)}&collectionType=${l.type}&paths=${encodeURIComponent(l.path)}`;
    // Always send a real JSON body: an empty body with a JSON content-type is a 400 on Hermit.
    const body = TARGET === 'jellyfin' ? JSON.stringify(noRemote) : '{}';
    const r = http.post(`${BASE}/Library/VirtualFolders?${q}${TARGET === 'jellyfin' ? '&refreshLibrary=true' : ''}`, body, h);
    if (r.status >= 300) throw new Error(`[${TARGET}] add library "${l.name}" failed: ${r.status} ${r.body}`);
  }
  if (TARGET !== 'jellyfin') {
    const r = http.post(`${BASE}/Library/Refresh`, null, h);   // hermit: kick the scan
    if (r.status >= 300) throw new Error(`[${TARGET}] /Library/Refresh failed: ${r.status}`);
  }
}

// Jellyfin's first boot needs the startup wizard completed before AuthenticateByName works.
// /System/Info/Public 200s while migrations are still seeding, so the wizard can race first
// boot — retry the whole sequence until Complete sticks.
function jellyfinFirstRunWizard() {
  const jh = { headers: { 'Content-Type': 'application/json' } };
  for (let i = 0; i < 60; i++) {
    const cfg = http.post(`${BASE}/Startup/Configuration`,
      JSON.stringify({ UICulture: 'en-US', MetadataCountryCode: 'US', PreferredMetadataLanguage: 'en' }), jh);
    if (cfg.status < 300) {
      http.get(`${BASE}/Startup/User`);
      http.post(`${BASE}/Startup/User`, JSON.stringify({ Name: USER, Password: PASS }), jh);
      const done = http.post(`${BASE}/Startup/Complete`, null, jh);
      if (done.status < 300) return;
    }
    sleep(2);
  }
  throw new Error(`[${TARGET}] startup wizard never completed`);
}

export function setup() {
  if (TARGET === 'jellyfin') jellyfinFirstRunWizard();
  const ctx = authenticate();
  provision(ctx);
  const found = waitForScan(ctx);

  // Deterministic item pick: first movie by SortName on BOTH servers (ids differ, item is the
  // same). For the image row, prefer the first of those with a discovered Primary image —
  // otherwise a poster-less pick turns the row into a 404 microbenchmark.
  const items = http.get(`${BASE}/Items?userId=${ctx.userId}&Recursive=true&IncludeItemTypes=Movie&SortBy=SortName&Limit=200`, tokenHeaders(ctx.token)).json();
  const list = (items.Items || []);
  ctx.itemId = list[0] ? list[0].Id : '';
  const withImage = list.find((i) => i.ImageTags && i.ImageTags.Primary);
  ctx.imageItemId = withImage ? withImage.Id : ctx.itemId;
  ctx.itemsFound = found;

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
