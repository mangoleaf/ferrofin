#!/usr/bin/env node
// Real HTTP/WebSocket regression check. Node >=22; no npm dependencies.
// Only creates disposable data. Usage: node verify/scan-progress.mjs BINARY [LOG_EVERY]
// Add --measure for before/after wall time and event counts without fix assertions.
import { mkdtemp, mkdir, writeFile, chmod, open, rm, appendFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawn } from 'node:child_process';
import { createServer } from 'node:net';
import { once } from 'node:events';
import assert from 'node:assert/strict';

const binary = resolve(process.argv[2] || 'target/debug/ferrofin-server');
const cadence = Number(process.argv[3] || 100);
const measure = process.argv.includes('--measure');
const count = measure ? 200 : 12;
const dir = await mkdtemp(join(tmpdir(), 'ferrofin-scan-progress-'));
const listener = createServer();
listener.listen(0, '127.0.0.1');
await once(listener, 'listening');
const port = listener.address().port;
await new Promise(resolve => listener.close(resolve));
const base = `http://127.0.0.1:${port}`;
const media = join(dir, 'media');
await mkdir(media);
const files = [];
for (let i = 0; i < count; i++) {
    const title = `Progress Movie ${String(i).padStart(3, '0')} (2020)`;
    await mkdir(join(media, title));
    const file = join(media, title, `${title}.mkv`);
    await writeFile(file, Buffer.alloc(1024));
    files.push(file);
}
await writeFile(join(dir, 'ffmpeg'), '#!/bin/sh\necho "ffmpeg version 7.1.1"\n');
await writeFile(join(dir, 'ffprobe'), `#!/bin/sh
case "$*" in *-version*) echo 'ffprobe version 6.1.1
libavutil      58. 29.100
libavcodec     60. 31.102
libavformat    60. 16.100
libavdevice    60.  3.100
libavfilter     9. 12.100
libswscale      7.  5.100
libswresample   4. 12.100'; exit 0;; esac
while test -f "$PROGRESS_FIXTURE_DIR/hold"; do sleep 0.02; done
sleep ${measure ? 0 : 0.15}
echo '{"streams":[{"index":0,"codec_type":"video","codec_name":"h264","width":640,"height":360}],"format":{"format_name":"matroska,webm","duration":"60.000000","size":"1024","bit_rate":"1000"}}'
`);
for (const name of ['ffmpeg', 'ffprobe']) await chmod(join(dir, name), 0o755);
const log = await open(join(dir, 'server.log'), 'w');
const proc = spawn(binary, ['--data-dir', join(dir, 'data'), '--bind', '127.0.0.1', '--port', String(port)], {
    env: { ...process.env, FERROFIN_CONFIG_DIR: join(dir, 'config'), FERROFIN_CACHE_DIR: join(dir, 'cache'),
        FERROFIN_FFMPEG_PATH: join(dir, 'ffmpeg'), FERROFIN_FFPROBE_PATH: join(dir, 'ffprobe'),
        FERROFIN_ADMIN_PASSWORD: '', FERROFIN_SCAN_PROBE_CONCURRENCY: '1',
        FERROFIN_SCAN_PROGRESS_EVERY: String(cadence), PROGRESS_FIXTURE_DIR: dir },
    stdio: ['ignore', log.fd, log.fd]
});
const sockets = [];
let token;
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
async function until(check, description, timeout = 30000) {
    const start = performance.now();
    while (!await check()) {
        assert(proc.exitCode === null, `server exited: ${dir}/server.log`);
        assert(performance.now() - start < timeout, `timeout: ${description}; logs: ${dir}`);
        await sleep(20);
    }
}
async function api(path, body, method = body === undefined ? 'GET' : 'POST') {
    const response = await fetch(base + path, { method,
        headers: { Authorization: `MediaBrowser Client="progress-check", Device="test", DeviceId="progress-check", Version="1"${token ? `, Token="${token}"` : ''}`,
            'Content-Type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body),
        signal: AbortSignal.timeout(10000) });
    const text = await response.text();
    assert(response.ok, `${path}: ${response.status} ${text}`);
    return text ? JSON.parse(text) : null;
}
async function connect(legacy = false) {
    // Web 12's SDK uses ApiKey without deviceId; legacy clients use api_key.
    const query = legacy ? `api_key=${token}&deviceId=progress-check` : `ApiKey=${token}`;
    const socket = new WebSocket(`ws://127.0.0.1:${port}/socket?${query}`);
    sockets.push(socket);
    const frames = [];
    socket.addEventListener('message', event => {
        const frame = JSON.parse(event.data);
        frames.push({ ...frame, at: performance.now() });
        if (frame.MessageType === 'ForceKeepAlive') socket.send(JSON.stringify({ MessageType: 'KeepAlive' }));
    });
    await once(socket, 'open');
    socket.send(JSON.stringify({ MessageType: 'ScheduledTasksInfoStart', Data: legacy ? '1000,1000' : '0,1000' }));
    await until(() => frames.some(f => f.MessageType === 'ScheduledTasksInfo'), 'initial task snapshot');
    return { socket, frames };
}
const taskFrames = frames => frames.filter(f => f.MessageType === 'ScheduledTasksInfo')
    .flatMap(f => f.Data.filter(t => t.Key === 'RefreshLibrary'));
const refreshFrames = frames => frames.filter(f => f.MessageType === 'RefreshProgress');
const hold = () => writeFile(join(dir, 'hold'), '');
const release = () => rm(join(dir, 'hold'), { force: true });
try {
    await until(async () => { try { await api('/System/Info/Public'); return true; } catch { return false; } }, 'boot');
    token = (await api('/Users/AuthenticateByName', { Username: 'admin', Pw: '' })).AccessToken;
    await api('/Library/VirtualFolders?name=ProgressTest&collectionType=movies&refreshLibrary=false', {
        LibraryOptions: { PathInfos: [{ Path: media }], EnableRealtimeMonitor: false,
            EnableChapterImageExtraction: false, EnableTrickplayImageExtraction: false,
            TypeOptions: [{ Type: 'Movie', MetadataFetchers: [], ImageFetchers: [] }] }
    });
    const folder = (await api('/Library/VirtualFolders'))[0];
    const task = (await api('/ScheduledTasks')).find(t => t.Key === 'RefreshLibrary');
    const taskState = () => api(`/ScheduledTasks/${task.Id}`);
    const folderState = async () => (await api('/Library/VirtualFolders'))[0];
    const startTask = () => api(`/ScheduledTasks/Running/${task.Id}`, undefined, 'POST');
    const settled = async () => {
        await until(async () => (await taskState()).State === 'Idle', 'task completion');
        assert.equal((await taskState()).LastExecutionResult.Status, 'Completed');
    };
    // Measure both binaries using the legacy protocol; old binaries cannot
    // authenticate the SDK socket whose regression the assertion runs cover.
    const { socket, frames } = await connect(measure);
    frames.length = 0;
    if (!measure) await hold();
    const start = performance.now();
    await startTask();
    if (!measure) {
        await until(() => refreshFrames(frames).length >= 3, 'start and repeated zero ticks');
        assert.equal(taskFrames(frames)[0].State, 'Running');
        assert.equal(taskFrames(frames)[0].CurrentProgressPercentage, 0);
        for (const frame of refreshFrames(frames)) {
            assert.equal(frame.Data.Progress, '0.00');
            assert.equal(frame.Data.RefreshStatus, 'Active');
            assert.equal(frame.Data.ItemId, folder.ItemId);
        }
        const live = await folderState();
        assert.equal(live.RefreshStatus, 'Active');
        assert.equal(live.RefreshProgress, 0);
        const reconnect = await connect(true);
        assert.equal(taskFrames(reconnect.frames)[0].State, 'Running');
        assert.equal(taskFrames(reconnect.frames)[0].CurrentProgressPercentage, 0);
        reconnect.socket.close();
    }
    await release();
    await settled();
    await until(() => refreshFrames(frames).some(f => f.Data.Progress === '100.00'), 'terminal library event');
    const initial = { ms: performance.now() - start, events: refreshFrames(frames).length };
    if (!measure) {
        const progress = refreshFrames(frames);
        assert(progress.some(f => Number(f.Data.Progress) > 0 && Number(f.Data.Progress) < 100));
        for (const frame of progress) {
            const completed = Number(frame.Data.Progress) * count / 100;
            assert(Math.abs(completed - Math.round(completed)) < 0.001, 'completed/total ratio');
        }
        assert.equal(progress.at(-1).Data.RefreshStatus, 'Idle');
        assert.equal((await folderState()).RefreshStatus, 'Idle');
        assert.equal((await folderState()).RefreshProgress, undefined);
        await until(() => taskFrames(frames).at(-1)?.State === 'Idle', 'terminal task snapshot');
    }
    const firstFrames = structuredClone(frames);
    frames.length = 0;
    const rescanStart = performance.now();
    await startTask();
    await settled();
    if (!measure) {
        await until(() => taskFrames(frames).some(t => t.State === 'Running') && taskFrames(frames).at(-1)?.State === 'Idle', 'short unchanged scan lifecycle');
        assert.equal(taskFrames(frames)[0].CurrentProgressPercentage, 0);
    }
    const unchanged = { ms: performance.now() - rescanStart, events: refreshFrames(frames).length };
    if (!measure) {
        // Scoped full refresh: library state must advance without running the global task.
        frames.length = 0;
        await hold();
        await api(`/Items/${folder.ItemId}/Refresh?Recursive=true&MetadataRefreshMode=FullRefresh&ImageRefreshMode=None`, undefined, 'POST');
        await until(() => refreshFrames(frames).length >= 2, 'scoped refresh ticks');
        assert.equal((await folderState()).RefreshStatus, 'Active');
        assert.equal((await taskState()).State, 'Idle');
        await release();
        await until(() => refreshFrames(frames).at(-1)?.Data.RefreshStatus === 'Idle', 'scoped refresh finish');
        // Change real files so the scheduled scan probes again, then cancel while held.
        for (const file of files) await appendFile(file, 'changed');
        frames.length = 0;
        await hold();
        await startTask();
        await until(() => refreshFrames(frames).length >= 2, 'cancel fixture active');
        await api(`/ScheduledTasks/Running/${task.Id}`, undefined, 'DELETE');
        await release();
        await until(async () => (await folderState()).RefreshStatus === 'Idle', 'cancel cleanup');
        await until(() => taskFrames(frames).at(-1)?.State === 'Idle', 'cancel task snapshot');
        assert.equal((await taskState()).LastExecutionResult.Status, 'Cancelled');
        assert.equal(refreshFrames(frames).at(-1).Data.Progress, '0.00');
        const countAtStop = refreshFrames(frames).length;
        await sleep(1200);
        assert.equal(refreshFrames(frames).length, countAtStop, 'no late tick after cancellation');
        // Empty library still emits start and finish, without division by zero.
        await rm(media, { recursive: true });
        await mkdir(media);
        frames.length = 0;
        await startTask();
        await settled();
        await until(() => refreshFrames(frames).at(-1)?.Data.RefreshStatus === 'Idle', 'empty scan finish');
        assert.equal(refreshFrames(frames)[0].Data.Progress, '0.00');
        assert.equal(refreshFrames(frames).at(-1).Data.Progress, '100.00');
    }
    const result = { binary, cadence, count, measure, initial, unchanged, dir,
        refreshEvents: refreshFrames(firstFrames).map(f => ({ ...f.Data, ms: f.at - start })) };
    await writeFile(join(dir, 'result.json'), JSON.stringify(result, null, 2));
    console.log(JSON.stringify(result, null, 2));
    socket.close();
} finally {
    await release();
    for (const socket of sockets) socket.close();
    proc.kill('SIGTERM');
    await Promise.race([once(proc, 'exit'), sleep(5000).then(() => { if (proc.exitCode === null) proc.kill('SIGKILL'); })]);
    await log.close();
}
