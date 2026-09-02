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
// identical by construction.
import http from 'k6/http';
import exec from 'k6/execution';
import { Trend, Rate, Counter } from 'k6/metrics';

const URL = __ENV.URL;
const IDS = JSON.parse(open(__ENV.IDS));
const SHAPE = __ENV.SHAPE === '1';
const SEED = Number(__ENV.SEED || 0);  // per-phase offset so a warm-up never replays the window's exact picks
const U = IDS.user;
const HDR = { Authorization: `MediaBrowser Client="bench", Device="bench", DeviceId="bench-k6", Version="3", Token="${IDS.token}"` };
const JSON_HDR = Object.assign({ 'Content-Type': 'application/json' }, HDR);
const IMAGES_PER_SCREEN = 12;

// weighted mix (D4): home 3 : movies 2 : detail 2 : series 1 : search 1 : playback 1
const MIX = ['home', 'movies', 'detail', 'home', 'movies', 'detail', 'home', 'series', 'search', 'playback'];

export const options = SHAPE
    ? { scenarios: { shape: { executor: 'shared-iterations', vus: 1, iterations: MIX.length } } }
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
const lat = {}, ok = {};
const mid = (n) => n.replace(/[^A-Za-z0-9_]/g, '_');
for (const n of NAMES.concat(SCREENS)) { lat[n] = new Trend(`lat_${mid(n)}`, true); ok[n] = new Rate(`ok_${mid(n)}`); }
const requests = new Counter('requests');
let iterOk = true;  // AND of the current screen's request statuses

function lcg(seed) { let s = (seed * 2654435761) >>> 0; return () => ((s = (Math.imul(s, 1664525) + 1013904223) >>> 0) / 4294967296); }
function q(p) { return Object.entries(p).map(([k, v]) => `${k}=${encodeURIComponent(v)}`).join('&'); }
// Map keys that are data, not schema (image tags, item ids, provider ids), collapse to {key}.
const DATA_KEY = /^[0-9a-f]{16,}$/i;
function fieldSet(v, out, prefix) {
    // union over every array element: item shape varies with data (logo on 20 % of movies, …)
    if (Array.isArray(v)) { for (const e of v) fieldSet(e, out, prefix + '[]'); return; }
    if (v && typeof v === 'object') for (const k of Object.keys(v)) { const kk = DATA_KEY.test(k) ? '{key}' : k; out.add(prefix + '.' + kk); fieldSet(v[k], out, prefix + '.' + kk); }
}
function record(name, res) {
    const good = res.status >= 200 && res.status < 400;
    lat[name].add(res.timings.duration);
    ok[name].add(good);
    if (!good) iterOk = false;
    requests.add(1);
    if (!SHAPE) return;
    const s = { status: res.status };
    if ((res.headers['Content-Type'] || '').includes('json')) {
        let body = null;
        try { body = res.json(); } catch (e) { /* shape of a non-JSON body: status only */ }
        if (body !== null) {
            const f = new Set(); fieldSet(body, f, ''); s.fields = Array.from(f).sort();
            s.count = Array.isArray(body) ? body.length : body.TotalRecordCount !== undefined ? body.TotalRecordCount : Array.isArray(body.Items) ? body.Items.length : undefined;
        }
    } else { s.bytes = res.body ? res.body.length : 0; }
    // VU state is invisible to handleSummary; shape lines go out via the console log
    // (run with --log-format json --console-output shape.log; report.py parses them).
    console.log(JSON.stringify(Object.assign({ shape: name, url: res.url }, s)));
}
function batch(reqs) {
    // reqs: [name, method, path, body?]
    const rs = http.batch(reqs.map(([n, m, p, b]) => [m, URL + p, b ? JSON.stringify(b) : null, { headers: b ? JSON_HDR : HDR, tags: { name: n } }]));
    rs.forEach((r, i) => record(reqs[i][0], r));
    return rs;
}
function get(name, path) { const r = http.get(URL + path, { headers: HDR, tags: { name } }); record(name, r); return r; }
function post(name, path, body) { const r = http.post(URL + path, JSON.stringify(body), { headers: JSON_HDR, tags: { name } }); record(name, r); return r; }
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
    // Deterministic pools shared by every server: first 500 movies / 100 series by SortName
    // (gen.py makes sort names unique, so the Limit boundary has no ties to break).
    const m = http.get(URL + `/Users/${U}/Items?` + q({ IncludeItemTypes: 'Movie', Recursive: true, SortBy: 'SortName', SortOrder: 'Ascending', Limit: 500, Fields: 'ImageTags', ParentId: IDS.movies_view }), { headers: HDR }).json();
    const s = http.get(URL + `/Users/${U}/Items?` + q({ IncludeItemTypes: 'Series', Recursive: true, SortBy: 'SortName', SortOrder: 'Ascending', Limit: 100, ParentId: IDS.shows_view }), { headers: HDR }).json();
    const words = new Set();
    for (const it of m.Items) for (const w of it.Name.split(' ')) if (w.length > 3 && w !== 'The') words.add(w);
    return { movies: m.Items.map(i => i.Id), series: s.Items.map(i => i.Id), movieCount: m.TotalRecordCount, terms: Array.from(words).sort().slice(0, 40) };
}

const screens = {
    // src/components/homesections/sections/{libraryTiles,resume,nextUp,recentlyAdded}.ts
    home(pool, rnd) {
        const cutoff = new Date(Date.now() - 365 * 86400000).toISOString();
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
        const pages = Math.max(1, Math.floor(pool.movieCount / 100));
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
        }
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
        try { ms = pi.json('MediaSources.0.Id') || id; ps = pi.json('PlaySessionId') || ps; } catch (e) { /* keep fallbacks */ }
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
    const name = MIX[i % MIX.length];
    exec.vu.tags.screen = name;
    const t0 = Date.now();
    iterOk = true;
    screens[name](pool, lcg(i + 1 + SEED));
    lat[name].add(Date.now() - t0);
    ok[name].add(iterOk);
}

export function handleSummary(data) {
    const m = data.metrics;
    const pick = (n) => {
        const t = m[`lat_${mid(n)}`], r = m[`ok_${mid(n)}`];
        if (!t) return null;
        return { count: t.values.count, p50: t.values['p(50)'], p95: t.values['p(95)'], p99: t.values['p(99)'], max: t.values.max, ok: r ? r.values.rate : null };
    };
    const out = {
        url: URL, rate: __ENV.RATE || null, duration: __ENV.DURATION || null, shape: SHAPE,
        dropped_iterations: m.dropped_iterations ? m.dropped_iterations.values.count : 0,
        iterations: m.iterations ? m.iterations.values.count : 0,
        requests: m.requests ? m.requests.values.count : 0,
        screens: Object.fromEntries(SCREENS.map(s => [s, pick(s)]).filter(([, v]) => v)),
        endpoints: Object.fromEntries(NAMES.map(n => [n, pick(n)]).filter(([, v]) => v)),
    };
    return { [__ENV.OUT || 'k6.json']: JSON.stringify(out, null, 1), stdout: SHAPE ? '' : `\n${Object.entries(out.screens).map(([s, v]) => `${s.padEnd(9)} p50 ${v.p50.toFixed(0)}ms p95 ${v.p95.toFixed(0)}ms p99 ${v.p99.toFixed(0)}ms n=${v.count}`).join('\n')}\ndropped=${out.dropped_iterations}\n` };
}
