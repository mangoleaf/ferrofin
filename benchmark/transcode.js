// EXPERIMENTAL, low-signal: time-to-first-HLS-segment. Both servers shell out to the same
// ffmpeg, so this measures pipeline/playlist overhead BEFORE ffmpeg output, not throughput.
// Single item, single VU. Reports N/A gracefully if the HLS request isn't 200 (device-profile
// / codec negotiation differs across servers and versions — deliberately not modelled here).
// ponytail: no deviceProfile negotiation. Add one only if the naive request 4xx's on both.
import http from 'k6/http';
import { Trend } from 'k6/metrics';

const BASE = __ENV.BASE_URL, TARGET = __ENV.TARGET;
const USER = __ENV.BENCH_ADMIN_USER || 'bench', PASS = __ENV.BENCH_ADMIN_PASSWORD || 'benchpass123';
export const options = { scenarios: { ttfs: { executor: 'per-vu-iterations', vus: 1, iterations: 5, maxDuration: '2m' } } };
const ttfs = new Trend('transcode_ttfs_ms', true);

const CLIENT_ID = 'Client="bench", Device="bench", DeviceId="bench", Version="1.0"';
const mb = (token) => ({ headers: { Authorization: `MediaBrowser Token="${token}", ${CLIENT_ID}` } });

function auth() {
  const r = http.post(`${BASE}/Users/AuthenticateByName`, JSON.stringify({ Username: USER, Pw: PASS }),
    { headers: { 'Content-Type': 'application/json', Authorization: `MediaBrowser ${CLIENT_ID}` } });
  return r.json();
}

export function setup() {
  const a = auth();
  const h = mb(a.AccessToken);
  // Pick the largest movie — your real 4K files dwarf any synthetic padding — so we
  // transcode something heavy, not a toy clip.
  const items = http.get(`${BASE}/Items?userId=${a.User.Id}&Recursive=true&IncludeItemTypes=Movie,Episode&Fields=MediaSources&Limit=200`, h).json();
  let best = '', bestSize = -1;
  for (const it of items.Items || []) {
    const sz = (it.MediaSources && it.MediaSources[0] && it.MediaSources[0].Size) || it.Size || 0;
    if (sz > bestSize) { bestSize = sz; best = it.Id; }
  }
  return { token: a.AccessToken, itemId: best };
}

export default function (ctx) {
  if (!ctx.itemId) return;
  const h = mb(ctx.token);
  const t0 = Date.now();
  const pl = http.get(`${BASE}/Videos/${ctx.itemId}/master.m3u8?mediaSourceId=${ctx.itemId}&videoCodec=h264&audioCodec=aac`, h);
  if (pl.status !== 200) { console.log(`[${TARGET}] HLS N/A (status ${pl.status})`); return; }
  const seg = (pl.body.match(/^[^#].*\.(ts|m4s|mp4).*$/m) || [])[0];
  if (!seg) return;
  const segUrl = seg.startsWith('http') ? seg : `${BASE}${seg.startsWith('/') ? '' : '/'}${seg}`;
  const s = http.get(segUrl, h);
  if (s.status === 200) ttfs.add(Date.now() - t0);
}

export function handleSummary(data) {
  const m = data.metrics.transcode_ttfs_ms;
  const out = { target: TARGET, ttfs_ms: m ? +m.values.med.toFixed(1) : null };
  return { [`results/raw/${TARGET}-transcode.json`]: JSON.stringify(out, null, 2), stdout: `${TARGET} TTFS: ${out.ttfs_ms ?? 'N/A'} ms\n` };
}
