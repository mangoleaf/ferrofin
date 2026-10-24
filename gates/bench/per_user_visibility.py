"""Disposable live parity and timing fixture for per-user item visibility.

Requires Python 3, ffmpeg/libx264, and a Ferrofin or Jellyfin executable.
Only files created under its own temporary directory are eligible for deletion.
Example: python3 bench/per_user_visibility.py ferrofin target/debug/ferrofin-server
Use --benchmark 100 --library-size 64 for warm-loopback latency samples.
Use --reference /path/to/jellyfin/results.json to assert equivalent observations.
"""
import argparse
import base64
import json
import os
import pathlib
import signal
import socket
import statistics
import struct
import subprocess
import tempfile
import time
import urllib.request
import urllib.error
import urllib.parse
import zlib
p = argparse.ArgumentParser()
p.add_argument('kind', choices=['ferrofin', 'jellyfin'])
p.add_argument('binary')
p.add_argument('--dotnet')
p.add_argument('--benchmark', type=int, default=0)
p.add_argument('--library-size', type=int, default=2)
p.add_argument('--reference')
args = p.parse_args()
if args.benchmark < 0 or args.library_size < 2:
    p.error('--benchmark must be nonnegative and --library-size must be at least 2')
if args.kind == 'jellyfin' and not args.dotnet:
    p.error('--dotnet is required for Jellyfin')
root = pathlib.Path(tempfile.mkdtemp(prefix='ferrofin-visibility-' + args.kind + '-'))
print('artifacts:', root, flush=True)
# Select an unused ephemeral loopback port for this disposable instance.
with socket.socket() as listener:
    listener.bind(('127.0.0.1', 0))
    port = listener.getsockname()[1]
password = 'Visibility-fixture-only-123!'

class Api:

    def __init__(self, token=None, user=None, device=None):
        self.token, self.user, self.device = (token, user, device or user or 'visibility-setup')

    def request(self, method, path, body=None, headers=None, **query):
        url = f'http://127.0.0.1:{port}' + path + ('?' + urllib.parse.urlencode(query) if query else '')
        auth = f'MediaBrowser Client="Visibility investigation", Device="Fixture", DeviceId="{self.device}", Version="1"'
        if self.token:
            auth += f', Token="{self.token}"'
        req = urllib.request.Request(url, data=(body if isinstance(body, bytes) else json.dumps(body).encode()) if body is not None else None, method=method, headers={'Authorization': auth, 'Content-Type': 'application/json', **(headers or {})})
        try:
            with urllib.request.urlopen(req, timeout=30) as r:
                code, raw = (r.status, r.read())
        except urllib.error.HTTPError as e:
            code, raw = (e.code, e.read())
        try:
            data = json.loads(raw) if raw else None
        except (ValueError, UnicodeDecodeError):
            data = None
        return (code, data)

    def ok(self, method, path, body=None, **query):
        code, data = self.request(method, path, body, **query)
        assert code in (200, 204), (method, path, code, data)
        return data

    def login(self, name):
        device = 'visibility-' + name
        data = Api(device=device).ok('POST', '/Users/AuthenticateByName', {'Username': name, 'Pw': password})
        return Api(data['AccessToken'], data['User']['Id'], device)
media = root / 'media'
for lib in ('Allowed', 'Hidden'):
    path = media / lib / lib
    path.mkdir(parents=True)
    subprocess.run(['ffmpeg', '-v', 'error', '-f', 'lavfi', '-i', 'color=c=blue:s=64x64:d=1', '-c:v', 'libx264', '-threads', '1', str(path / (lib + '.mkv'))], check=True)
for n in range(max(0, args.library_size - 2)):
    path = media / 'Allowed' / f'Sample{n:04d}'
    path.mkdir()
    os.link(media / 'Allowed' / 'Allowed' / 'Allowed.mkv', path / f'Sample{n:04d}.mkv')
pathlib.Path(root / 'config').mkdir()
env = {**os.environ, 'FERROFIN_ADMIN_USER': 'visibility-admin', 'FERROFIN_ADMIN_PASSWORD': password, 'HTTP_PROXY': 'http://127.0.0.1:9', 'HTTPS_PROXY': 'http://127.0.0.1:9', 'NO_PROXY': 'localhost,127.0.0.1'}
if args.kind == 'ferrofin':
    (root / 'config/network.json').write_text('{"AutoDiscovery":false}')
    cmd = [args.binary, '--data-dir', str(root), '--bind', '127.0.0.1', '--port', str(port)]
else:
    (root / 'config/network.xml').write_text(f'<NetworkConfiguration><InternalHttpPort>{port}</InternalHttpPort><AutoDiscovery>false</AutoDiscovery><EnableIPv6>false</EnableIPv6><EnableRemoteAccess>false</EnableRemoteAccess><LocalNetworkAddresses><string>127.0.0.1</string></LocalNetworkAddresses></NetworkConfiguration>')
    cmd = [args.dotnet, args.binary, '--datadir', str(root / 'data'), '--configdir', str(root / 'config'), '--cachedir', str(root / 'cache'), '--logdir', str(root / 'log'), '--nowebclient', '--ffmpeg', '/usr/bin/ffmpeg', '--nonetchange']
results = []
with (root / 'server.log').open('w') as log:
    proc = subprocess.Popen(cmd, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    try:
        a = Api()
        for _ in range(120):
            if proc.poll() is not None:
                raise RuntimeError('server exited; inspect ' + str(root / 'server.log'))
            try:
                code, info = a.request('GET', '/System/Info/Public')
                if code == 200 and isinstance(info, dict) and ('Version' in info):
                    break
            except OSError:
                pass
            time.sleep(1)
        else:
            raise RuntimeError('server readiness timeout')
        print('version:', info.get('Version', info.get('version', 'unknown')), flush=True)
        if args.kind == 'jellyfin':
            a.ok('POST', '/Startup/Configuration', {'UICulture': 'en-US', 'MetadataCountryCode': 'US', 'PreferredMetadataLanguage': 'en'})
            a.ok('GET', '/Startup/User')
            a.ok('POST', '/Startup/User', {'Name': 'visibility-admin', 'Password': password})
            a.ok('POST', '/Startup/RemoteAccess', {'EnableRemoteAccess': False, 'EnableAutomaticPortMapping': False})
            a.ok('POST', '/Startup/Complete')
        admin = a.login('visibility-admin')
        admin.ok('POST', '/Users/New', {'Name': 'visibility-restricted', 'Password': password})
        for lib in ('Allowed', 'Hidden'):
            options = {'PathInfos': [{'Path': str(media / lib)}], 'EnableRealtimeMonitor': False, 'EnableInternetProviders': False, 'EnableChapterImageExtraction': False, 'EnableTrickplayImageExtraction': False, 'TypeOptions': [{'Type': t, 'MetadataFetchers': [], 'ImageFetchers': []} for t in ('Movie', 'Video')]}
            admin.ok('POST', '/Library/VirtualFolders', {'LibraryOptions': options}, name=lib, collectionType='movies', refreshLibrary='false')
        admin.ok('POST', '/Library/Refresh')
        for _ in range(90):
            rows = admin.ok('GET', '/Items', userId=admin.user, recursive='true', includeItemTypes='Movie')['Items']
            if len(rows) == max(2, args.library_size) and all((t['State'] == 'Idle' for t in admin.ok('GET', '/ScheduledTasks'))):
                break
            time.sleep(1)
        assert len(rows) == max(2, args.library_size), rows
        items = {r['Name']: r['Id'] for r in rows}
        folders = {r['Name']: r['ItemId'] for r in admin.ok('GET', '/Library/VirtualFolders')}
        user = next((u for u in admin.ok('GET', '/Users') if u['Name'] == 'visibility-restricted'))
        default = dict(user['Policy'])

        def policy(**changes):
            admin.ok('POST', f"/Users/{user['Id']}/Policy", {**default, **changes})

        def probe(label, client, method, path, body=None, **query):
            code, data = client.request(method, path, body, **query)
            row = {'case': label, 'method': method, 'path': path, 'status': code}
            if isinstance(data, dict) and 'Items' in data:
                row['names'] = [x.get('Name') for x in data['Items']]
            if isinstance(data, dict) and 'MediaSources' in data:
                row['media_sources'] = len(data['MediaSources'])
            if isinstance(data, list) and label.startswith('SyncPlay'):
                row['groups'] = len(data)
            results.append(row)
            print(label, code, flush=True)
        policy(EnableAllFolders=False, EnabledFolders=[folders['Allowed']], EnableContentDeletion=True, EnableContentDownloading=True)
        restricted = a.login('visibility-restricted')
        hidden = items['Hidden']
        allowed = items['Allowed']

        def png_chunk(kind, data):
            return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))
        png = b'\x89PNG\r\n\x1a\n' + png_chunk(b'IHDR', struct.pack('>IIBBBBB', 2, 2, 8, 2, 0, 0, 0)) + png_chunk(b'IDAT', zlib.compress(b'\x00\xff\x00\x00\xff\x00\x00' * 2)) + png_chunk(b'IEND', b'')
        for image_item in (allowed, hidden):
            admin.ok('POST', f'/Items/{image_item}/Images/Primary', base64.b64encode(png), headers={'Content-Type': 'image/png'})
        admin.ok('GET', f'/Items/{hidden}/Images/Primary', tag='visibility-fixture')
        probe('hidden image', restricted, 'GET', f'/Items/{hidden}/Images/Primary', tag='visibility-fixture')
        probe('hidden image HEAD', restricted, 'HEAD', f'/Items/{hidden}/Images/Primary', tag='visibility-fixture')
        probe('hidden conditional image', restricted, 'GET', f'/Items/{hidden}/Images/Primary', tag='visibility-fixture', headers={'If-None-Match': '"visibility-fixture"'})
        probe('allowed image', restricted, 'GET', f'/Items/{allowed}/Images/Primary', tag='visibility-fixture')
        probe('restricted browse', restricted, 'GET', '/Items', userId=restricted.user, recursive='true', includeItemTypes='Movie')
        probe('allowed detail', restricted, 'GET', '/Items/' + allowed)
        for label, method, suffix, body in [('hidden detail', 'GET', '', None), ('hidden playback', 'POST', '/PlaybackInfo', {}), ('hidden download', 'GET', '/Download', None), ('hidden ancestors', 'GET', '/Ancestors', None), ('hidden extras', 'GET', '/SpecialFeatures', None), ('hidden similar', 'GET', '/Similar', None)]:
            probe(label, restricted, method, '/Items/' + hidden + suffix, body)
        probe('hidden favorite', restricted, 'POST', '/UserFavoriteItems/' + hidden)
        probe('hidden userdata', restricted, 'GET', '/UserItems/' + hidden + '/UserData')
        probe('hidden played', restricted, 'POST', '/UserPlayedItems/' + hidden)
        probe('admin detail', admin, 'GET', '/Items/' + hidden)
        probe('admin targeting restricted user', admin, 'GET', '/Items/' + hidden, userId=restricted.user)
        detail = admin.ok('GET', '/Items/' + allowed)
        detail.update({'Tags': ['Restricted'], 'OfficialRating': 'R', 'CustomRating': None})
        admin.ok('POST', '/Items/' + allowed, detail)
        policy(EnableAllFolders=True, BlockedTags=['Restricted'])
        probe('blocked tag detail', restricted, 'GET', '/Items/' + allowed)
        policy(EnableAllFolders=True, AllowedTags=['Unmatched'])
        probe('unmatched allowed tag detail', restricted, 'GET', '/Items/' + allowed)
        policy(EnableAllFolders=True, MaxParentalRating=1)
        probe('parental limit detail', restricted, 'GET', '/Items/' + allowed)
        policy(EnableAllFolders=False, EnabledFolders=[folders['Allowed']], EnableContentDeletion=True)
        probe('hidden explicit ids query', restricted, 'GET', '/Items', userId=restricted.user, ids=hidden, recursive='true')
        probe('hidden parent query', restricted, 'GET', '/Items', userId=restricted.user, parentId=folders['Hidden'], recursive='true')
        admin.ok('POST', '/Auth/Keys', app='Visibility fixture')
        keys = admin.ok('GET', '/Auth/Keys')
        key = Api(next((k['AccessToken'] for k in keys['Items'] if k['AppName'] == 'Visibility fixture')))
        probe('API key download', key, 'GET', '/Items/' + hidden + '/Download')
        probe('API key targeting restricted user', key, 'GET', '/Items/' + hidden, userId=restricted.user)
        policy(EnableAllFolders=False, EnabledFolders=[folders['Allowed']], IsAdministrator=True, EnableContentDeletion=True)
        restricted = a.login('visibility-restricted')
        probe('restricted admin detail', restricted, 'GET', '/Items/' + hidden)
        policy(EnableAllFolders=False, EnabledFolders=[folders['Allowed']], EnableContentDeletion=True)
        restricted = a.login('visibility-restricted')
        playlist = admin.ok('POST', '/Playlists', {'Name': 'Private fixture', 'UserId': admin.user, 'IsPublic': False, 'Ids': [allowed]})['Id']
        probe('private playlist detail', restricted, 'GET', '/Items/' + playlist)
        probe('private playlist delete', restricted, 'DELETE', '/Items/' + playlist)
        admin_policy = admin.ok('GET', f'/Users/{admin.user}')['Policy']
        admin_policy['SyncPlayAccess'] = 'CreateAndJoinGroups'
        admin.ok('POST', f'/Users/{admin.user}/Policy', admin_policy)
        policy(EnableAllFolders=False, EnabledFolders=[folders['Allowed']], EnableContentDeletion=True, SyncPlayAccess='CreateAndJoinGroups')
        group = admin.ok('POST', '/SyncPlay/New', {'GroupName': 'Visibility fixture'})
        admin.ok('POST', '/SyncPlay/SetNewQueue', {'PlayingQueue': [hidden], 'PlayingItemPosition': 0, 'StartPositionTicks': 0})
        probe('SyncPlay hidden group list', restricted, 'GET', '/SyncPlay/List')
        probe('SyncPlay hidden group detail', restricted, 'GET', '/SyncPlay/' + group['GroupId'])
        queue = [r['Id'] for r in rows if r['Id'] != hidden]
        admin.ok('POST', '/SyncPlay/SetNewQueue', {'PlayingQueue': queue, 'PlayingItemPosition': 0, 'StartPositionTicks': 0})
        probe('SyncPlay allowed group list', restricted, 'GET', '/SyncPlay/List')
        benchmarks = []
        if args.benchmark:
            for label, path, query in [('detail', '/Items/' + allowed, {}), ('PlaybackInfo', '/Items/' + allowed + '/PlaybackInfo', {}), ('image hit', '/Items/' + allowed + '/Images/Primary', {'tag': 'visibility-fixture'}), ('SyncPlay list', '/SyncPlay/List', {})]:
                first = time.perf_counter()
                restricted.ok('GET', path, **query)
                first = (time.perf_counter() - first) * 1000
                for _ in range(10):
                    restricted.ok('GET', path, **query)
                durations = []
                for _ in range(args.benchmark):
                    started = time.perf_counter()
                    restricted.ok('GET', path, **query)
                    durations.append((time.perf_counter() - started) * 1000)
                durations.sort()
                benchmarks.append({'route': label, 'requests': len(durations), 'first_ms': first, 'p50_ms': statistics.median(durations), 'p95_ms': durations[(95 * len(durations) + 99) // 100 - 1], 'mean_ms': statistics.mean(durations)})
            (root / 'benchmark.json').write_text(json.dumps({'library_items': len(rows), 'queue_items': len(queue), 'measurements': benchmarks}, indent=2) + '\n')
            print('benchmark:', benchmarks, flush=True)
        probe('hidden delete', restricted, 'DELETE', '/Items/' + hidden)
        probe('admin detail after delete', admin, 'GET', '/Items/' + hidden)
        results.append({'case': 'media exists after delete', 'exists': (media / 'Hidden' / 'Hidden' / 'Hidden.mkv').exists()})
        (root / 'results.json').write_text(json.dumps({'kind': args.kind, 'version': info.get('Version', info.get('version', 'unknown')), 'results': results}, indent=2) + '\n')
        if args.reference:
            reference = json.loads(pathlib.Path(args.reference).read_text())['results']
            expected = {row['case']: {k: v for k, v in row.items() if k in ('status', 'names', 'media_sources', 'groups', 'exists')} for row in reference}
            observed = {row['case']: {k: v for k, v in row.items() if k in ('status', 'names', 'media_sources', 'groups', 'exists')} for row in results}
            assert observed == expected, {'different_cases': {label: {'observed': value, 'expected': expected.get(label)} for label, value in observed.items() if value != expected.get(label)}}
    finally:
        if proc.poll() is None:
            os.killpg(proc.pid, signal.SIGTERM)
        try:
            proc.wait(timeout=20)
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.wait()
