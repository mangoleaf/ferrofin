// Transcode time-to-first-segment (TTFS), measured as a real client experiences it:
//   POST PlaybackInfo (real Chrome DeviceProfile, DirectPlay/DirectStream disabled)
//   -> TranscodingUrl -> master.m3u8 -> variant -> init (if fMP4) + first media segment.
//
// Two modes per iteration, each its own metric:
//   copy   — profile as-is: the Chrome HLS profile accepts hevc, so both servers negotiate
//            a video stream-copy remux. This is realistic "time to play start".
//   encode — TranscodingProfile pinned to h264 + AllowVideoStreamCopy=false: a genuine
//            4K HEVC -> H.264 software encode, the heavy pipeline path.
//
// Fairness:
// - fresh DeviceId per measurement: both servers key transcode sessions/caches on device,
//   so a reused id can serve cached segments and fake a near-zero TTFS.
// - DELETE /Videos/ActiveEncodings after each measurement so a lingering ffmpeg doesn't
//   steal CPU from the next one.
// - target = movie with the longest runtime (a real 4K film, never a synthetic clip);
//   runtime comes from probing the identical file, so both servers pick the same title.
import http from 'k6/http';
import { sleep } from 'k6';
import { Trend } from 'k6/metrics';

const BASE = __ENV.BASE_URL, TARGET = __ENV.TARGET;
const USER = __ENV.BENCH_ADMIN_USER || 'bench', PASS = __ENV.BENCH_ADMIN_PASSWORD || 'benchpass123';
const ITERATIONS = parseInt(__ENV.TTFS_ITERATIONS || '3', 10);
// Encode-mode bitrate cap — jellyfin-web's "1080p" quality rung. Being far below the 4K
// source bitrate is what forces a genuine re-encode on both servers (stream copy would
// exceed the cap): Hermit ignores the Allow*StreamCopy flags today (parity gap), but the
// bitrate condition is honored by both.
const ENCODE_BITRATE = parseInt(__ENV.TTFS_BITRATE || '8000000', 10);
// The repo's real Chrome device profile fixture — single source of truth.
const PROFILE = JSON.parse(open('../crates/hermit-model/tests/data/DeviceProfile-Chrome.json'));
// Encode mode: same profile but video transcode pinned to h264, so stream copy is impossible.
const PROFILE_H264 = JSON.parse(JSON.stringify(PROFILE));
for (const t of PROFILE_H264.TranscodingProfiles || [])
  if (t.Type === 'Video') t.VideoCodec = 'h264';

export const options = {
  summaryTrendStats: ['med', 'min', 'max', 'count'],
  scenarios: { ttfs: { executor: 'per-vu-iterations', vus: 1, iterations: ITERATIONS, maxDuration: '30m' } },
};
const trends = {
  copy: new Trend('ttfs_copy_ms', true),
  encode: new Trend('ttfs_encode_ms', true),
};
const LONG = { timeout: '240s' };   // first 4K encoded segment on 4 capped CPUs takes a while

const clientId = (dev) => `Client="bench", Device="${dev}", DeviceId="${dev}", Version="1.0"`;
const mb = (token, dev) => ({ headers: { Authorization: `MediaBrowser Token="${token}", ${clientId(dev)}`, 'Content-Type': 'application/json' } });

function auth(dev) {
  // Reuse a token minted before the perf load if the runner passed one: the auth_login
  // scenario throttles Jellyfin's login, so a fresh auth here would fail (500/429).
  if (__ENV.CAP_TOKEN) return { token: __ENV.CAP_TOKEN, userId: __ENV.CAP_UID };
  const r = http.post(`${BASE}/Users/AuthenticateByName`, JSON.stringify({ Username: USER, Pw: PASS }),
    { headers: { 'Content-Type': 'application/json', Authorization: `MediaBrowser ${clientId(dev)}` } });
  if (r.status !== 200) throw new Error(`[${TARGET}] ttfs auth failed: ${r.status}`);
  const b = r.json();
  return { token: b.AccessToken, userId: b.User.Id };
}

// Resolve a playlist reference (absolute URL, absolute path, or relative) against the playlist URL.
function resolve(playlistUrl, ref) {
  if (ref.startsWith('http')) return ref;
  if (ref.startsWith('/')) return `${BASE}${ref}`;
  return playlistUrl.split('?')[0].replace(/[^/]*$/, '') + ref;
}

export function setup() {
  const a = auth('bench-ttfs-setup');
  const h = mb(a.token, 'bench-ttfs-setup');
  const r = http.get(`${BASE}/Items?userId=${a.userId}&Recursive=true&IncludeItemTypes=Movie&Limit=600`, h);
  if (r.status !== 200) throw new Error(`[${TARGET}] ttfs item query failed: ${r.status} ${String(r.body).slice(0, 200)}`);
  const items = r.json().Items || [];
  let best = null;
  for (const it of items) {
    const ticks = it.RunTimeTicks || 0;
    if (!best || ticks > (best.RunTimeTicks || 0) ||
        (ticks === (best.RunTimeTicks || 0) && it.Name < best.Name)) best = it;
  }
  if (!best || !(best.RunTimeTicks > 0)) throw new Error(`[${TARGET}] ttfs: no movie with a runtime found (${items.length} items)`);
  console.log(`[${TARGET}] ttfs target: "${best.Name}" (${Math.round(best.RunTimeTicks / 600_000_000)} min)`);
  return { userId: a.userId, itemId: best.Id };
}

// Unique per run: both servers key transcode caches on DeviceId, and a device name reused
// from a *previous* run serves that run's cached segments (observed: 40 ms "transcodes").
const RUN_ID = Date.now().toString(36);

// One full client play-start against a fresh device; returns elapsed ms or null (reason logged).
function measure(ctx, mode, iter) {
  const dev = `bench-ttfs-${RUN_ID}-${mode}-${iter}`;
  const a = auth(dev);
  const h = mb(a.token, dev);
  const fail = (msg) => { console.log(`[${TARGET}] ${mode}#${iter} ${msg}`); return null; };

  const t0 = Date.now();
  const pi = http.post(`${BASE}/Items/${ctx.itemId}/PlaybackInfo?userId=${ctx.userId}`,
    JSON.stringify({
      DeviceProfile: mode === 'encode' ? PROFILE_H264 : PROFILE,
      MediaSourceId: ctx.itemId,
      MaxStreamingBitrate: mode === 'encode' ? ENCODE_BITRATE : undefined,
      EnableDirectPlay: false,
      EnableDirectStream: false,
      EnableTranscoding: true,
      AllowVideoStreamCopy: mode !== 'encode',
      AllowAudioStreamCopy: mode !== 'encode',
      AutoOpenLiveStream: true,
    }), h);
  if (pi.status !== 200) return fail(`PlaybackInfo ${pi.status}: ${String(pi.body).slice(0, 300)}`);
  const src = (pi.json().MediaSources || [])[0] || {};
  if (!src.TranscodingUrl) return fail(`no TranscodingUrl (SupportsTranscoding=${src.SupportsTranscoding})`);

  let ms = null;
  const masterUrl = resolve(BASE, src.TranscodingUrl);
  const master = http.get(masterUrl, Object.assign({}, h, LONG));
  if (master.status !== 200) return fail(`master.m3u8 ${master.status}`);
  const variantRef = (String(master.body).match(/^[^#\s].*$/m) || [])[0];
  if (!variantRef) return fail('master.m3u8 had no variant');

  const variantUrl = resolve(masterUrl, variantRef);
  const variant = http.get(variantUrl, Object.assign({}, h, LONG));
  if (variant.status !== 200) return fail(`variant ${variant.status}`);
  const body = String(variant.body);

  // fMP4 HLS needs the init segment before the first media segment — part of TTFS.
  const initRef = (body.match(/#EXT-X-MAP:URI="([^"]+)"/) || [])[1];
  if (initRef) {
    const init = http.get(resolve(variantUrl, initRef), Object.assign({}, h, LONG));
    if (init.status !== 200) return fail(`init segment ${init.status}`);
  }
  const segRef = (body.match(/^[^#\s].*$/m) || [])[0];
  if (!segRef) return fail('variant had no segment line');
  const seg = http.get(resolve(variantUrl, segRef), Object.assign({}, h, LONG));
  if (seg.status !== 200) return fail(`first segment ${seg.status}`);
  ms = Date.now() - t0;
  console.log(`[${TARGET}] ttfs ${mode}#${iter}: ${ms} ms (segment ${(seg.body.length / 1e6).toFixed(1)} MB)`);

  // Kill this measurement's ffmpeg so it can't contend with the next one. Twice: once by
  // playSessionId (the jellyfin-web way), once device-scoped with an empty psid — Hermit's
  // jobs don't carry a PlaySessionId yet, so only the device-scoped form matches there.
  const psid = pi.json().PlaySessionId;
  if (psid) http.del(`${BASE}/Videos/ActiveEncodings?deviceId=${dev}&playSessionId=${psid}`, null, h);
  http.del(`${BASE}/Videos/ActiveEncodings?deviceId=${dev}&playSessionId=`, null, h);
  sleep(3);
  return ms;
}

export default function (ctx) {
  for (const mode of ['copy', 'encode']) {
    const ms = measure(ctx, mode, __ITER);
    if (ms !== null) trends[mode].add(ms);
  }
}

export function handleSummary(data) {
  const stat = (name) => {
    const v = (data.metrics[name] || {}).values;
    return v && v.count ? { med: +v.med.toFixed(0), min: +v.min.toFixed(0), max: +v.max.toFixed(0), runs: v.count } : null;
  };
  const out = { target: TARGET, copy: stat('ttfs_copy_ms'), encode: stat('ttfs_encode_ms'), iterations: ITERATIONS };
  const show = (s) => (s ? `${s.med} ms (${s.runs}/${ITERATIONS})` : 'N/A');
  return {
    [`results/raw/${TARGET}-transcode.json`]: JSON.stringify(out, null, 2),
    stdout: `${TARGET} TTFS copy: ${show(out.copy)} · encode: ${show(out.encode)}\n`,
  };
}
