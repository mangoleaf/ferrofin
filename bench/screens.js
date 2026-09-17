// The six screens a jellyfin-web 10.11.8 user waits for, as the request sets the web
// client issues (PLAN_BENCHMARK_V3 §2 D3). Each iteration = "a user opens screen X",
// requests issued concurrently as the browser does (http.batch), dependent requests
// (images of the returned cards) after. Sources are cited per screen.
//
//   load:   k6 run -e URL=… -e IDS=ids.json -e RATE=5 -e DURATION=120s -e SEED=0 -e OUT=k6-loaded.json screens.js
//   shape:  k6 run -e URL=… -e IDS=ids.json -e SHAPE=1 -e OUT=shape.json screens.js
//
// Iteration i always opens the same screen with the same picks on every server
// (weighted round-robin + a seeded LCG), so the arrival pattern and the work are
// shared for primary picks. Dependent selections are observed and compared explicitly.
import http from 'k6/http';
import exec from 'k6/execution';
import { Trend, Rate, Counter } from 'k6/metrics';

const URL = __ENV.URL;
const IDS = JSON.parse(open(__ENV.IDS));
const SHAPE = __ENV.SHAPE === '1';
const SEED = Number(__ENV.SEED || 0);
const SLOTS = Number(__ENV.SLOTS || 600);
const SHAPE_VUS = Number(__ENV.SHAPE_VUS || 1);
if (!Number.isInteger(SHAPE_VUS) || SHAPE_VUS <= 0) throw new Error('SHAPE_VUS must be a positive integer');
if (!Number.isInteger(SLOTS) || SLOTS <= 0 || SLOTS % 10) throw new Error('SLOTS must be a positive multiple of ten');
const U = IDS.user;
const HDR = { Authorization: `MediaBrowser Client="bench", Device="bench", DeviceId="bench-k6", Version="3", Token="${IDS.token}"` };
const JSON_HDR = Object.assign({ 'Content-Type': 'application/json' }, HDR);
const IMAGES_PER_SCREEN = 12;

// weighted mix (D4): home 3 : movies 2 : detail 2 : series 1 : search 1 : playback 1
const MIX = ['home', 'movies', 'detail', 'home', 'movies', 'detail', 'home', 'series', 'search', 'playback'];

export const options = SHAPE
    ? { scenarios: { shape: { executor: 'shared-iterations', vus: SHAPE_VUS, iterations: SLOTS, maxDuration: '10m' } } }
    : {
        scenarios: {
            screens: {
                executor: 'constant-arrival-rate', rate: Number(__ENV.RATE), timeUnit: '1s',
                duration: __ENV.DURATION, preAllocatedVUs: 50, maxVUs: 2000,
            },
        },
        summaryTrendStats: ['p(50)', 'p(95)', 'p(99)', 'max', 'count'],
    };

// One Trend + Rate per request name and per screen: the per-endpoint and per-screen
// tables come straight out of handleSummary, no raw-point post-processing.
const NAMES = [
    'home:views', 'home:resume-video', 'home:resume-audio', 'home:resume-book', 'home:nextup',
    'home:latest-movies', 'home:latest-shows', 'home:latest-music',
    'movies:items', 'detail:item', 'detail:similar', 'detail:special-features', 'detail:local-trailers',
    'series:item', 'series:seasons', 'series:episodes', 'series:similar',
    'search:items', 'search:videos', 'search:persons', 'search:artists', 'search:programs',
    'playback:playbackinfo', 'playback:intros', 'playback:segments', 'playback:playing', 'playback:stopped',
    'image',
];
const SCREENS = ['home', 'movies', 'detail', 'series', 'search', 'playback'];
NAMES.push(...SCREENS.filter(s => s !== 'playback').map(s => s + ':image'));
const lat = {}, ok = {};
const mid = (n) => n.replace(/[^A-Za-z0-9_]/g, '_');
for (const n of NAMES.concat(SCREENS)) { lat[n] = new Trend(`lat_${mid(n)}`, true); ok[n] = new Rate(`ok_${mid(n)}`); }
const requests = new Counter('requests');
const imageBytes = new Counter('image_bytes');
let slot = 0, screen = '', occurrences = {}, selections = [];
let iterOk = true;  // AND of the current screen's request statuses

function lcg(seed) { let s = (seed * 2654435761) >>> 0; return () => ((s = (Math.imul(s, 1664525) + 1013904223) >>> 0) / 4294967296); }
function q(p) { return Object.entries(p).map(([k, v]) => `${k}=${encodeURIComponent(v)}`).join('&'); }
// Retain each item's field types; array indices preserve duplicates and order.
const DATA_KEY = /^[0-9a-f]{16,}$/i;
function fieldTypes(v, out, prefix = '$') {
    out[prefix] = v === null ? 'null' : Array.isArray(v) ? `array(${v.length})` : typeof v;
    if (Array.isArray(v)) v.forEach((x, i) => fieldTypes(x, out, `${prefix}[${i}]`));
    else if (v && typeof v === 'object') for (const k of Object.keys(v).sort())
        fieldTypes(v[k], out, prefix + '.' + (DATA_KEY.test(k) ? '{key}' : k));
}
function requestKey(name, method, path, body) {
    const [route, query = ''] = path.split('?');
    // Image cache tags and playback session IDs vary between equivalent servers.
    const stableQuery = query.split('&').filter(x => x && !['tag', 'apikey', 'api_key', 'playsessionid'].includes(x.split('=')[0].toLowerCase())).sort();
    const stableBody = Object.assign({}, body || {});
    delete stableBody.PlaySessionId;
    const occurrence = occurrences[name] || 0;
    occurrences[name] = occurrence + 1;
    return JSON.stringify([slot, name, occurrence, method, route + (stableQuery.length ? '?' + stableQuery.join('&') : ''), stableBody]);
}
function record(name, res, method, path, body) {
    const isImage = name === 'image';
    const scoped = isImage ? screen + ':image' : name;
    const key = requestKey(scoped, method, path, body);
    selections.push(key);
    const contentType = (res.headers['Content-Type'] || '').split(';')[0].trim().toLowerCase();
    const bytes = isImage && res.body ? res.body.byteLength : 0;
    const good = res.status >= 200 && res.status < 300 && (!isImage || (bytes > 0 && contentType.startsWith('image/')));
    for (const n of isImage ? [name, scoped] : [name]) {
        lat[n].add(res.timings.duration); ok[n].add(good);
    }
    if (!good) iterOk = false;
    requests.add(1);
    if (isImage) imageBytes.add(bytes);
    if (!SHAPE) return;
    const observation = { shape: scoped, slot, key, status: res.status, content_type: contentType };
    if (contentType.includes('json')) {
        try {
            const body = res.json();
            const types = {}; fieldTypes(body, types); observation.types = types;
            const rows = Array.isArray(body) ? body : body && Array.isArray(body.Items) ? body.Items : null;
            if (rows) { observation.count = rows.length; observation.ids = rows.map(x => x && x.Id !== undefined ? x.Id : null); }
            else if (body && body.Id !== undefined) observation.ids = [body.Id];
            if (body && body.TotalRecordCount !== undefined) observation.total = body.TotalRecordCount;
        } catch (e) { observation.invalid_json = true; iterOk = false; }
    }
    if (isImage) observation.bytes = bytes;
    console.log(JSON.stringify(observation));
}
function params(name, body) {
    return { headers: body ? JSON_HDR : HDR, tags: { name }, responseType: name === 'image' ? 'binary' : 'text' };
}
function batch(reqs) {
    const rs = http.batch(reqs.map(([n, m, p, b]) => [m, URL + p, b ? JSON.stringify(b) : null, params(n, b)]));
    rs.forEach((r, i) => record(reqs[i][0], r, reqs[i][1], reqs[i][2], reqs[i][3]));
    return rs;
}
function get(name, path) { const r = http.get(URL + path, params(name)); record(name, r, 'GET', path); return r; }
function post(name, path, body) { const r = http.post(URL + path, JSON.stringify(body), params(name, body)); record(name, r, 'POST', path, body); return r; }
function items(res) { try { const b = res.json(); return Array.isArray(b) ? b : b.Items ? b.Items : b.Id ? [b] : []; } catch (e) { return []; } }
function images(responses) {
    // the posters the cards would load — jellyfin-web card image URL (fillHeight/fillWidth/quality/tag)
    const reqs = [];
    for (const r of responses) for (const it of items(r)) {
        if (reqs.length >= IMAGES_PER_SCREEN) break;
        if (it.ImageTags && it.ImageTags.Primary) reqs.push(['image', 'GET', `/Items/${it.Id}/Images/Primary?fillHeight=300&fillWidth=200&quality=96&tag=${it.ImageTags.Primary}`]);
    }
    if (reqs.length) batch(reqs);
}

const CARD = { Fields: 'PrimaryImageAspectRatio', ImageTypeLimit: 1, EnableImageTypes: 'Primary,Backdrop,Thumb', EnableTotalRecordCount: false };
const SEARCH = { Fields: 'PrimaryImageAspectRatio,CanDelete,MediaSourceCount', enableTotalRecordCount: false, imageTypeLimit: 1, userId: U };
const PROFILE = {
    Name: 'bench', MaxStreamingBitrate: 120000000, MaxStaticBitrate: 100000000, MusicStreamingTranscodingBitrate: 384000,
    DirectPlayProfiles: [
        { Container: 'mkv,mp4,m4v,webm', Type: 'Video', VideoCodec: 'h264,hevc,av1,vp9', AudioCodec: 'aac,mp3,ac3,eac3,flac,opus' },
        { Container: 'mp3,m4a,flac,ogg,opus', Type: 'Audio' },
    ],
    TranscodingProfiles: [
        { Container: 'ts', Type: 'Video', AudioCodec: 'aac', VideoCodec: 'h264', Context: 'Streaming', Protocol: 'hls', MaxAudioChannels: '2', MinSegments: 1, BreakOnNonKeyFrames: true },
        { Container: 'mp3', Type: 'Audio', AudioCodec: 'mp3', Context: 'Streaming', Protocol: 'http' },
    ],
    CodecProfiles: [], ResponseProfiles: [],
    SubtitleProfiles: [{ Format: 'vtt', Method: 'External' }, { Format: 'srt', Method: 'External' }, { Format: 'ass', Method: 'External' }],
};

export function setup() {
    const pool = IDS.pools;
    if (!pool || !['movies', 'series', 'terms'].every(k => Array.isArray(pool[k]) && pool[k].length) ||
        !Number.isInteger(pool.movieCount) || pool.movieCount <= 0 || !pool.nextUpCutoff)
        throw new Error('fixture pools missing or invalid; run build.sh --export-pools');
    return pool;
}

const screens = {
    // src/components/homesections/sections/{libraryTiles,resume,nextUp,recentlyAdded}.ts
    home(pool, rnd) {
        const cutoff = pool.nextUpCutoff;
        const rs = batch([
            ['home:views', 'GET', `/Users/${U}/Views`],
            ['home:resume-video', 'GET', `/Users/${U}/Items/Resume?` + q({ Limit: 12, Recursive: true, ...CARD, MediaTypes: 'Video' })],
            ['home:resume-audio', 'GET', `/Users/${U}/Items/Resume?` + q({ Limit: 12, Recursive: true, ...CARD, MediaTypes: 'Audio' })],
            ['home:resume-book', 'GET', `/Users/${U}/Items/Resume?` + q({ Limit: 12, Recursive: true, ...CARD, MediaTypes: 'Book' })],
            ['home:nextup', 'GET', `/Shows/NextUp?` + q({ Limit: 24, Fields: 'PrimaryImageAspectRatio,DateCreated,Path,MediaSourceCount', UserId: U, ImageTypeLimit: 1, EnableImageTypes: 'Primary,Backdrop,Banner,Thumb', EnableTotalRecordCount: false, DisableFirstEpisode: false, NextUpDateCutoff: cutoff, EnableResumable: false, EnableRewatching: false })],
            ['home:latest-movies', 'GET', `/Users/${U}/Items/Latest?` + q({ Limit: 16, Fields: 'PrimaryImageAspectRatio,Path', ImageTypeLimit: 1, EnableImageTypes: 'Primary,Backdrop,Thumb', ParentId: IDS.movies_view })],
            ['home:latest-shows', 'GET', `/Users/${U}/Items/Latest?` + q({ Limit: 16, Fields: 'PrimaryImageAspectRatio,Path', ImageTypeLimit: 1, EnableImageTypes: 'Primary,Backdrop,Thumb', ParentId: IDS.shows_view })],
            ['home:latest-music', 'GET', `/Users/${U}/Items/Latest?` + q({ Limit: 16, Fields: 'PrimaryImageAspectRatio,Path', ImageTypeLimit: 1, EnableImageTypes: 'Primary,Backdrop,Thumb', ParentId: IDS.music_view })],
        ]);
        images([rs[1], rs[4], rs[5], rs[6]]);
    },
    // src/controllers/movies/movies.js — a random page of the library (page size 100)
    movies(pool, rnd) {
        const pages = Math.max(1, Math.ceil(pool.movieCount / 100));
        const rs = batch([['movies:items', 'GET', `/Users/${U}/Items?` + q({ SortBy: 'SortName,ProductionYear', SortOrder: 'Ascending', IncludeItemTypes: 'Movie', Recursive: true, Fields: 'PrimaryImageAspectRatio,MediaSourceCount', ImageTypeLimit: 1, EnableImageTypes: 'Primary,Backdrop,Banner,Thumb', StartIndex: Math.floor(rnd() * pages) * 100, Limit: 100, ParentId: IDS.movies_view })]]);
        images(rs);
    },
    // src/controllers/itemDetails/index.js — item, similar, special features, local trailers
    detail(pool, rnd) {
        const id = pool.movies[Math.floor(rnd() * pool.movies.length)];
        const rs = batch([
            ['detail:item', 'GET', `/Users/${U}/Items/${id}`],
            ['detail:similar', 'GET', `/Items/${id}/Similar?` + q({ userId: U, limit: 12, fields: 'PrimaryImageAspectRatio,CanDelete' })],
            ['detail:special-features', 'GET', `/Users/${U}/Items/${id}/SpecialFeatures`],
            ['detail:local-trailers', 'GET', `/Users/${U}/Items/${id}/LocalTrailers`],
        ]);
        images([rs[0], rs[1]]);
    },
    // src/controllers/itemDetails/index.js renderChildren — series → seasons → first season's episodes
    series(pool, rnd) {
        const id = pool.series[Math.floor(rnd() * pool.series.length)];
        const f = 'ItemCounts,PrimaryImageAspectRatio,CanDelete,MediaSourceCount';
        const rs = batch([
            ['series:item', 'GET', `/Users/${U}/Items/${id}`],
            ['series:seasons', 'GET', `/Shows/${id}/Seasons?` + q({ userId: U, Fields: f })],
            ['series:similar', 'GET', `/Items/${id}/Similar?` + q({ userId: U, limit: 12, fields: 'PrimaryImageAspectRatio,CanDelete' })],
        ]);
        const seasons = items(rs[1]);
        if (seasons.length) {
            const ep = get('series:episodes', `/Shows/${id}/Episodes?` + q({ seasonId: seasons[0].Id, userId: U, Fields: f + ',Overview' }));
            images([rs[1], ep]);
        } else { iterOk = false; }
    },
    // src/apps/stable/features/search/api/* — the global search request set
    search(pool, rnd) {
        const term = pool.terms[Math.floor(rnd() * pool.terms.length)];
        const rs = batch([
            ['search:items', 'GET', `/Items?` + q({ ...SEARCH, recursive: true, includeItemTypes: 'Movie,Series,Episode,Playlist,MusicAlbum,Audio,TvChannel,PhotoAlbum,Photo,AudioBook,Book,BoxSet', searchTerm: term, isMissing: false, limit: 800 })],
            ['search:videos', 'GET', `/Items?` + q({ ...SEARCH, recursive: true, mediaTypes: 'Video', excludeItemTypes: 'Movie,Episode,TvChannel', searchTerm: term, limit: 100 })],
            ['search:persons', 'GET', `/Persons?` + q({ ...SEARCH, searchTerm: term, limit: 100 })],
            ['search:artists', 'GET', `/Artists?` + q({ ...SEARCH, searchTerm: term, limit: 100 })],
            ['search:programs', 'GET', `/Items?` + q({ ...SEARCH, recursive: true, includeItemTypes: 'LiveTvProgram', searchTerm: term, limit: 100 })],
        ]);
        images([rs[0]]);
    },
    // src/components/playback/playbackmanager.js — start (direct play) and stop right away
    playback(pool, rnd) {
        const id = pool.movies[Math.floor(rnd() * pool.movies.length)];
        const pi = post('playback:playbackinfo', `/Items/${id}/PlaybackInfo?` + q({ UserId: U, StartTimeTicks: 0, IsPlayback: true, AutoOpenLiveStream: true, MaxStreamingBitrate: 120000000 }), { DeviceProfile: PROFILE });
        let ms = id, ps = `bench${Math.floor(rnd() * 1e9)}`;
        try { ms = pi.json('MediaSources.0.Id'); ps = pi.json('PlaySessionId'); if (!ms || !ps) iterOk = false; } catch (e) { iterOk = false; }
        batch([
            ['playback:intros', 'GET', `/Users/${U}/Items/${id}/Intros`],
            ['playback:segments', 'GET', `/MediaSegments/${id}?includeSegmentTypes=Intro&includeSegmentTypes=Outro&includeSegmentTypes=Recap&includeSegmentTypes=Preview&includeSegmentTypes=Commercial`],
        ]);
        const base = { ItemId: id, MediaSourceId: ms, PlaySessionId: ps, PlayMethod: 'DirectPlay', CanSeek: true, IsPaused: false, IsMuted: false, VolumeLevel: 100, RepeatMode: 'RepeatNone' };
        post('playback:playing', '/Sessions/Playing', Object.assign({ PositionTicks: 0 }, base));
        post('playback:stopped', '/Sessions/Playing/Stopped', Object.assign({ PositionTicks: 0 }, base));
    },
};

export default function (pool) {
    const i = exec.scenario.iterationInTest;
    slot = i % SLOTS;
    const name = MIX[slot % MIX.length];
    screen = name; occurrences = {}; selections = [];
    exec.vu.tags.screen = name;
    const t0 = Date.now();
    iterOk = true;
    screens[name](pool, lcg(slot + 1 + SEED));
    lat[name].add(Date.now() - t0);
    ok[name].add(iterOk);
    // Compact selection evidence is emitted after the screen latency is recorded.
    console.log(JSON.stringify({ selection: slot, iteration: i, screen, keys: selections, ok: iterOk }));
}

export function handleSummary(data) {
    const m = data.metrics;
    const pick = (n) => {
        const t = m[`lat_${mid(n)}`], r = m[`ok_${mid(n)}`];
        if (!t) return null;
        return { count: t.values.count, p50: t.values['p(50)'], p95: t.values['p(95)'], p99: t.values['p(99)'], max: t.values.max, ok: r ? r.values.rate : null };
    };
    const out = {
        workload: 4, slots: SLOTS, seed: SEED, shape_vus: SHAPE_VUS, elapsed_ms: data.state.testRunDurationMs,
        image_bytes: m.image_bytes ? m.image_bytes.values.count : 0,
        url: URL, rate: __ENV.RATE || null, duration: __ENV.DURATION || null, shape: SHAPE,
        dropped_iterations: m.dropped_iterations ? m.dropped_iterations.values.count : 0,
        iterations: m.iterations ? m.iterations.values.count : 0,
        requests: m.requests ? m.requests.values.count : 0,
        screens: Object.fromEntries(SCREENS.map(s => [s, pick(s)]).filter(([, v]) => v)),
        endpoints: Object.fromEntries(NAMES.map(n => [n, pick(n)]).filter(([, v]) => v)),
    };
    return { [__ENV.OUT || 'k6.json']: JSON.stringify(out, null, 1), stdout: SHAPE ? '' : `\n${Object.entries(out.screens).map(([s, v]) => `${s.padEnd(9)} p50 ${v.p50.toFixed(0)}ms p95 ${v.p95.toFixed(0)}ms p99 ${v.p99.toFixed(0)}ms n=${v.count}`).join('\n')}\ndropped=${out.dropped_iterations}\n` };
}
