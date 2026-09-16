"""Reporter correctness only: python3 -m unittest discover -s bench -p test_report.py."""
import copy
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import Mock, patch
import urllib.error

import report
import coldstart
import mem_sample
import ttfs
from testdata import seed


class ReportTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.a = self.make_run('a')
        self.b = self.make_run('b')

    def write(self, path, value):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(value))

    def edit(self, path, change):
        value = json.loads(path.read_text())
        change(value)
        self.write(path, value)

    def make_run(self, name):
        root = self.root / name
        self.write(root / 'run.json', {
            'name': name, 'sha': 'abc', 'date': '2026-09-04', 'cpu': 'test',
            'host': 'test', 'memory_limit': '8g', 'server_cpus': '0,2-3',
            'rate_loaded': 5, 'window_s': 120, 'k6': 'test',
            'testdata_counts': {'movies': 100, 'series': 10, 'episodes': 100},
        })
        for server, _ in report.SERVERS:
            d = root / server
            d.mkdir()
            tag = {'jellyfin12': 'jellyfin/jellyfin:12.0-rc7',
                   'jellyfin': 'jellyfin/jellyfin:10.11.8', 'ferrofin': 'ferrofin:bench'}[server]
            (d / 'image.txt').write_text(f'{tag} sha256:{server}')
            value = {'count': 60, 'p50': 10 if server == 'ferrofin' else 20,
                     'p95': 30, 'p99': 40, 'max': 50, 'ok': 1}
            endpoints = {f'{s}:items': copy.deepcopy(value) for s in report.SCREENS}
            endpoints['image'] = copy.deepcopy(value)
            self.write(d / 'k6-loaded.json', {
                'iterations': 600, 'requests': 700, 'dropped_iterations': 0,
                'rate': '5', 'duration': '120s',
                'screens': {s: copy.deepcopy(value) for s in report.SCREENS},
                'endpoints': endpoints,
            })
            self.write(d / 'counts.json', {'movies': 100})
            records = [{'shape': n, 'url': f'http://{server}/{n}', 'status': 200,
                        'count': 1, 'fields': ['.Items', '.Items[].Id'], 'bytes': 100}
                       for n in endpoints]
            self.shapes(d, records)
            self.write(d / 'coldstart.json', {'runs': [{'home_ms': 100} for _ in range(5)]})
            self.write(d / 'ttfs.json', {
                'hls': [{'ttfs_ms': 100, 'transcoding_url': '/master.m3u8?VideoCodec=h264'} for _ in range(5)],
                'direct': [{'ttfb_ms': 1, 'status': 206} for _ in range(5)],
            })
            self.write(d / 'windows.json', {'loaded': {'start': 1, 'end': 2}, 'steady': {'start': 3, 'end': 4}})
            (d / 'mem.csv').write_text('t,anon,file,current,swap\n1,1048576,0,1048576,0\n2,2097152,0,2097152,0\n3,1048576,0,1048576,0\n4,1048576,0,1048576,0\n')
        return root

    def shapes(self, directory, records):
        (directory / 'shape.log').write_text('\n'.join(json.dumps({'msg': json.dumps(r)}) for r in records))

    def build(self, *runs):
        return report.build([str(r) for r in runs or (self.a,)])

    def home(self, model, server='ferrofin'):
        return dict(model['levels']['loaded']['screens'])['home'][server]

    def readme(self, model=None):
        model = model or self.build()
        return report.render_readme(model, model['runs'])

    def row(self, text, label='home'):
        return next(line for line in text.splitlines() if line.startswith(f'| **{label}**'))

    def test_clean_report_and_optional_servers(self):
        m = self.build(self.a, self.b)
        self.assertIsNone(self.home(m).flag)
        self.assertIn('faster on 7', self.readme(m))
        self.assertIn('3 logical CPUs', self.readme(m))
        self.assertIn('12.0-rc7', report.render_md(m))
        self.assertIn('No measured request failed', self.readme(m))
        self.assertNotIn('full runs', self.readme(m))
        self.assertIn('<html', report.render_html(m))

    def test_later_missing_shape_and_selection_order(self):
        (self.b / 'ferrofin/shape.log').unlink()
        forward, reverse = self.build(self.a, self.b), self.build(self.b, self.a)
        self.assertIn('no shape pass', self.home(forward).flag)
        self.assertEqual(self.home(forward).flag, self.home(reverse).flag)
        self.assertNotIn('faster', self.row(self.readme(forward)))

    def test_http_failures_invalidate_measured_window(self):
        self.edit(self.a / 'ferrofin/k6-loaded.json', lambda x: x['screens']['home'].update(ok=0.5))
        m = self.build()
        self.assertIn('failed', self.home(m).flag)
        self.assertNotIn('No measured request failed', self.readme(m))
        self.assertNotIn('faster', self.row(self.readme(m)))

    def test_oracle_http_failure_invalidates_comparison(self):
        self.edit(self.b / 'jellyfin12/k6-loaded.json', lambda x: x['endpoints']['home:items'].update(ok=0))
        m = self.build(self.a, self.b)
        self.assertIn('failed', self.home(m).flag)
        self.assertIn('failed', self.home(m, 'jellyfin12').flag)

    def test_slower_and_tied_candidates_do_not_claim_wins(self):
        for latency, sentence in [(20, 'about the same on 7'), (25, 'slower on 7')]:
            with self.subTest(latency=latency):
                def update(x):
                    for group in ('screens', 'endpoints'):
                        for v in x[group].values():
                            v['p50'] = latency
                self.edit(self.a / 'ferrofin/k6-loaded.json', update)
                self.assertIn(sentence, self.readme())
                self.assertNotIn('faster on 7', self.readme())

    def test_mixed_conditions_and_images_are_rejected(self):
        original = json.loads((self.b / 'run.json').read_text())
        for key, value in [('rate_loaded', 50), ('window_s', 5), ('memory_limit', '1g'),
                           ('server_cpus', '4-7'), ('ids_sha256', 'changed'), ('screens_sha256', 'changed')]:
            with self.subTest(key=key):
                self.write(self.b / 'run.json', {**original, key: value})
                with self.assertRaisesRegex(ValueError, key):
                    self.build(self.a, self.b)
        self.write(self.b / 'run.json', original)
        (self.b / 'jellyfin12/image.txt').write_text('jellyfin/jellyfin:12.0 sha256:different')
        with self.assertRaisesRegex(ValueError, 'jellyfin12 image'):
            self.build(self.a, self.b)

    def test_actual_window_conditions_are_checked(self):
        self.edit(self.b / 'ferrofin/k6-loaded.json', lambda x: x.update(rate='50'))
        with self.assertRaisesRegex(ValueError, 'loaded rate'):
            self.build(self.a, self.b)

    def test_baseline_allows_candidate_build_only(self):
        (self.b / 'ferrofin/image.txt').write_text('ferrofin:bench sha256:next')
        self.assertTrue(report.comparable_levels(self.build(self.a), self.build(self.b)))
        self.assertIn('<html', report.render_html(self.build(self.a), self.build(self.b)))
        with self.assertRaisesRegex(ValueError, 'ferrofin image'):
            self.build(self.a, self.b)
        self.edit(self.b / 'run.json', lambda x: x.update(window_s=10))
        with self.assertRaisesRegex(ValueError, 'baseline'):
            report.render_html(self.build(self.a), self.build(self.b))

    def test_failed_and_truncated_timing_repetitions(self):
        self.edit(self.b / 'ferrofin/coldstart.json', lambda x: x['runs'][1].update(home_ms=None))
        self.edit(self.b / 'ferrofin/ttfs.json', lambda x: x.update(direct=x['direct'][:1]))
        m = self.build(self.a, self.b)
        for name, cells in m['ttfs']:
            if name in report.README_TTFS:
                self.assertIn('successful repetitions', cells['ferrofin'].flag)
                self.assertIsNotNone(cells['ferrofin'].median)

    def test_known_nondefault_repetition_count(self):
        self.edit(self.a / 'run.json', lambda x: x.update(restarts=1))
        self.edit(self.a / 'ferrofin/coldstart.json', lambda x: x.update(runs=x['runs'][:1]))
        self.assertIsNone(dict(self.build()['ttfs'])[report.README_TTFS[0]]['ferrofin'].flag)

    def test_swapped_counts_between_request_keys_fail(self):
        for server, counts in [('jellyfin12', [1, 2]), ('ferrofin', [2, 1])]:
            self.shapes(self.a / server, [{'shape': 'movies:items', 'url': f'http://{server}/Items?page={i}',
                                         'status': 200, 'count': count, 'fields': ['.Items']}
                                        for i, count in enumerate(counts)])
        self.assertIsNotNone(report.comparable(report.load_shape(self.a / 'ferrofin'),
                                              report.load_shape(self.a / 'jellyfin12'), ['movies:items']))

    def test_image_byte_sizes_do_not_flag_cells(self):
        for server, size in [('jellyfin12', 100), ('ferrofin', 200)]:
            path = self.a / server / 'shape.log'
            records = [json.loads(json.loads(line)['msg']) for line in path.read_text().splitlines()]
            for r in records:
                if r['shape'] == 'image':
                    r['bytes'] = size
            self.shapes(path.parent, records)
        m = self.build()
        self.assertIsNone(dict(m['levels']['loaded']['endpoints'])['image']['ferrofin'].flag)
        self.assertIn('faster', self.row(self.readme(m)))

    def test_third_server_caveat_withholds_its_cell(self):
        path = self.a / 'jellyfin/shape.log'
        records = [json.loads(json.loads(line)['msg']) for line in path.read_text().splitlines()]
        records[0]['fields'] = []
        self.shapes(path.parent, records)
        m = self.build()
        self.assertIsNotNone(self.home(m, 'jellyfin').flag)
        row = self.row(self.readme(m))
        self.assertIn('— ⚠', row)
        self.assertIn('faster', row)  # FF/oracle still valid

    def test_missing_counts_and_later_inventory_difference(self):
        self.edit(self.b / 'ferrofin/counts.json', lambda x: x.update(movies=1))
        m = self.build(self.a, self.b)
        self.assertIn('inventory', self.home(m).flag)
        self.assertTrue(any('count movies' in x for x in m['work']['ferrofin']))
        (self.b / 'ferrofin/counts.json').unlink()
        self.assertIn('missing counts', self.home(self.build(self.a, self.b)).flag)

    def test_transport_failure_in_oracle_shape(self):
        self.shapes(self.a / 'jellyfin12', [{'shape': 'home:items', 'url': '/home:items', 'status': 0}])
        self.assertIsNotNone(self.home(self.build()).flag)

    def test_invalid_quantile_cannot_be_published(self):
        self.edit(self.a / 'ferrofin/k6-loaded.json', lambda x: x['screens']['home'].update(p50=float('nan')))
        m = self.build()
        self.assertIsNotNone(self.home(m).flag)
        self.assertNotIn('nan', self.row(self.readme(m)))

    def test_missing_timing_file_is_flagged(self):
        (self.b / 'ferrofin/coldstart.json').unlink()
        c = dict(self.build(self.a, self.b)['ttfs'])[report.README_TTFS[0]]['ferrofin']
        self.assertIsNotNone(c.flag)
        self.assertEqual(c.runs, 2)
        self.assertEqual(len(c.vals), 1)

    def test_two_servers_and_single_server_render(self):
        shutil.rmtree(self.a / 'jellyfin')
        self.assertNotIn('**Jellyfin 10.11.8**', self.readme())
        self.assertIn('**Jellyfin 12.0-rc7**', self.readme())
        shutil.rmtree(self.a / 'jellyfin12')
        text = self.readme()
        self.assertIn('no Jellyfin reference selected', text)
        self.assertNotIn('faster', self.row(text))

    def test_dropped_and_aborted_work_withholds_resource_comparisons(self):
        for key in ('dropped_iterations', 'aborted_iterations'):
            with self.subTest(key=key):
                self.edit(self.a / 'ferrofin/k6-loaded.json', lambda x: x.update({key: 1}))
                m = self.build()
                self.assertIsNotNone(self.home(m).flag)
                for name, cells in m['memory']:
                    if name in report.README_MEMORY:
                        self.assertIsNotNone(cells['ferrofin'].flag)
                self.edit(self.a / 'ferrofin/k6-loaded.json', lambda x: x.update({key: 0}))

    def test_missing_window_is_incomplete_not_zero_errors(self):
        (self.b / 'ferrofin/k6-loaded.json').unlink()
        m = self.build(self.a, self.b)
        self.assertIn('missing load window', self.home(m).flag)
        self.assertNotIn('No measured request failed', self.readme(m))

    def test_later_oracle_timing_failure_withholds_ratio(self):
        self.edit(self.b / 'jellyfin12/coldstart.json', lambda x: x['runs'][0].update(home_ms=None))
        text = self.readme(self.build(self.a, self.b))
        line = next(x for x in text.splitlines() if x.startswith('| cold start'))
        self.assertIn('— ⚠', line)
        self.assertNotIn('faster', line)

    def test_empty_image_is_not_accepted(self):
        for server in ('ferrofin', 'jellyfin12'):
            with self.subTest(server=server):
                path = self.a / server / 'shape.log'
                original = path.read_text()
                records = [json.loads(json.loads(line)['msg']) for line in original.splitlines()]
                for r in records:
                    if r['shape'] == 'image':
                        r['bytes'] = 0
                self.shapes(path.parent, records)
                c = dict(self.build()['levels']['loaded']['endpoints'])['image'][server]
                self.assertIsNotNone(c.flag)
                path.write_text(original)

    def test_missing_tail_withholds_entire_headline_cell(self):
        self.edit(self.a / 'ferrofin/k6-loaded.json', lambda x: x['screens']['home'].pop('p99'))
        self.assertNotIn('faster', self.row(self.readme()))

    def test_version_labels_follow_each_run_in_every_renderer(self):
        old = self.build(self.a)
        self.write(self.b / 'jellyfin12/system-info.json', {'Version': '12.0.0'})
        new = self.build(self.b)
        for render in (report.render_md, report.render_html,
                       lambda m: report.render_readme(m, m['runs'])):
            with self.subTest(renderer=render):
                self.assertIn('Jellyfin 12.0.0', render(new))
                self.assertNotIn('12.0-rc7', render(new))
                self.assertIn('Jellyfin 12.0-rc7', render(old))
                self.assertNotIn('Jellyfin 12.0.0', render(old))

    def test_digest_without_version_is_not_labelled_stable(self):
        d = self.a / 'jellyfin12'
        (d / 'image.txt').write_text('jellyfin/jellyfin@sha256:abc sha256:def')
        self.assertEqual(report.server_label(d, 'jellyfin12'), 'Jellyfin (version unknown)')

    def test_reported_version_mismatch_refuses_aggregation(self):
        self.write(self.a / 'jellyfin12/system-info.json', {'Version': '12.0.0'})
        with self.assertRaisesRegex(ValueError, 'jellyfin12 reported version'):
            self.build(self.a, self.b)

    def test_required_image_failure_withholds_its_screen(self):
        for server, _ in report.SERVERS:
            d = self.a / server
            records = [json.loads(json.loads(line)['msg']) for line in (d/'shape.log').read_text().splitlines()]
            for rec in records:
                if rec['shape'] == 'image':
                    rec.update(shape='home:image', key='poster', bytes=100, content_type='image/jpeg')
            self.shapes(d, records)
        self.assertIsNone(self.home(self.build()).flag)
        d = self.a/'ferrofin'
        records = [json.loads(json.loads(line)['msg']) for line in (d/'shape.log').read_text().splitlines()]
        for rec in records:
            if rec['shape'] == 'home:image':
                rec['bytes'] = 200
        self.shapes(d, records)
        self.assertIsNone(self.home(self.build()).flag)
        for rec in records:
            if rec['shape'] == 'home:image':
                rec['bytes'] = 0
        self.shapes(d, records)
        model = self.build()
        self.assertIn('empty', self.home(model).flag)
        self.assertNotIn('faster', self.row(self.readme(model)))

    def test_workload_hashes_and_seeds_cannot_be_mixed(self):
        for field in ('workload', 'ids_sha256', 'screens_sha256', 'seed', 'warmup_seed', 'slots', 'pools'):
            with self.subTest(field=field):
                self.edit(self.a/'run.json', lambda m: m.update({field: 4}))
                with self.assertRaisesRegex(ValueError, field):
                    self.build(self.a, self.b)
                self.edit(self.a/'run.json', lambda m: m.pop(field))

    def with_phases(self):
        self.edit(self.a/'run.json', lambda m: m.update(phase_schema=1))
        for server, _ in report.SERVERS:
            self.write(self.a/server/'phases.json', {n: {'status':'completed'} for n in
                ('startup','startup-drain','shape','drain-loaded','warmup-loaded','loaded','sampler','steady','coldstart','ttfs')})

    def test_failed_warmup_or_interrupted_window_flags_latency(self):
        self.with_phases()
        for name, status in (('warmup-loaded','failed'),('loaded','running')):
            self.edit(self.a/'ferrofin/phases.json', lambda p: p[name].update(status=status))
            self.assertIn(name,self.home(self.build()).flag)
            self.edit(self.a/'ferrofin/phases.json', lambda p: p[name].update(status='completed'))

    def test_skipped_selected_window_stays_identifiable(self):
        self.with_phases()
        self.write(self.a/'ferrofin/windows.json', {})
        (self.a/'ferrofin/k6-loaded.json').unlink()
        self.edit(self.a/'ferrofin/phases.json', lambda p: p['loaded'].update(status='skipped',reason='warm-up failed'))
        self.assertIn('loaded',report.run_levels(self.a/'ferrofin'))
        self.assertIn('skipped',self.home(self.build()).flag)

    def test_failed_sampler_preserves_valid_latency(self):
        self.with_phases()
        self.edit(self.a/'ferrofin/phases.json', lambda p: p['sampler'].update(status='failed'))
        model=self.build()
        self.assertIsNone(self.home(model).flag)
        self.assertIn('sampler', dict(model['memory'])['peak under load']['ferrofin'].flag)

    def test_earlier_transcode_repetition_is_compared(self):
        self.edit(self.a/'ferrofin/ttfs.json', lambda t: t['hls'][1].update(transcoding_url='/master.m3u8?VideoCodec=hevc'))
        model=self.build()
        cell=dict(model['ttfs'])['HLS first segment (forced transcode)']['ferrofin']
        self.assertIn('parameters differ',cell.flag)

    def test_stream_probe_failure_in_later_run_is_not_ignored(self):
        self.edit(self.b/'run.json', lambda m:m.update(streaming_validation=1))
        self.edit(self.a/'run.json', lambda m:m.update(streaming_validation=1))
        model=self.build(self.a,self.b)
        cell=dict(model['ttfs'])['HLS first segment (forced transcode)']['ferrofin']
        self.assertIn('validation evidence',cell.flag)

    def test_missing_memory_repetition_withholds_headline(self):
        (self.b / 'ferrofin/mem.csv').unlink()
        m = self.build(self.a, self.b)
        for name, cells in m['memory']:
            if name in report.README_MEMORY:
                self.assertIsNotNone(cells['ferrofin'].flag)


class PreparationTests(unittest.TestCase):
    def task(self, state='Idle', end='2026-09-14T01:00:00Z', status='Completed'):
        return {'Id': 'scan', 'Key': 'RefreshLibrary', 'State': state,
                'LastExecutionResult': {'EndTimeUtc': end, 'Status': status}}

    @patch.object(seed.time, 'sleep')
    def test_scan_waits_past_initial_idle_for_new_completion(self, sleep):
        done = self.task(end='2026-09-15T01:00:00Z')
        api = Mock()
        api.get.side_effect = [[self.task()], self.task(), self.task('Running'), done]
        self.assertEqual(seed.refresh_and_wait(api, 30), done['LastExecutionResult'])
        api.post.assert_called_once_with('/Library/Refresh')
        self.assertEqual(api.get.call_count, 4)
        self.assertEqual(sleep.call_count, 2)

    def test_failed_scan_is_not_ready(self):
        api = Mock()
        api.get.side_effect = [[self.task()], self.task(end='2026-09-15T01:00:00Z', status='Failed')]
        with self.assertRaisesRegex(RuntimeError, 'scan failed'):
            seed.refresh_and_wait(api, 30)

    @patch.object(seed.time, 'sleep')
    @patch.object(seed.time, 'monotonic', side_effect=[0, 1, 31])
    def test_stale_idle_times_out(self, monotonic, sleep):
        api = Mock()
        api.get.side_effect = [[self.task()], self.task()]
        with self.assertRaisesRegex(RuntimeError, 'deadline'):
            seed.refresh_and_wait(api, 30)

    def test_already_running_scan_is_not_adopted(self):
        api = Mock()
        api.get.return_value = [self.task('Running')]
        with self.assertRaisesRegex(RuntimeError, 'already active'):
            seed.refresh_and_wait(api, 30)
        api.post.assert_not_called()

    @patch.object(seed.time, 'sleep')
    @patch.object(seed.urllib.request, 'urlopen')
    def test_startup_503_remains_retryable(self, urlopen, sleep):
        urlopen.side_effect = urllib.error.HTTPError('http://localhost', 503, 'starting', {}, None)
        self.addCleanup(urlopen.side_effect.close)
        api = seed.Api('http://localhost', attempts=1)
        with self.assertRaises(urllib.error.HTTPError):
            api.get('/System/Info/Public')
        with patch.object(api, 'get', side_effect=[urlopen.side_effect, {'Version': '12.0.0'}]):
            seed.wait_ready(api, secs=1)
        sleep.assert_called_once_with(0.5)

    @patch.object(seed.time, 'sleep')
    def test_setup_server_200_is_not_api_readiness(self, sleep):
        api = Mock()
        api.get.side_effect = [{'version': '12.0.0'}, {'Version': '12.0.0'}]
        seed.wait_ready(api, secs=1)
        self.assertEqual(api.get.call_count, 2)
        sleep.assert_called_once_with(0.5)

    def test_invalid_selections_fail_before_fixture_or_docker_access(self):
        script = Path(__file__).parent / 'run.sh'
        for flag, value in (('--only', ''), ('--only', 'loaded,bogus'), ('--only', 'loaded,,shape'),
                            ('--only', 'loaded shape'), ('--only', ',counts'),
                            ('--servers', ''), ('--servers', 'jellyfin12,unknown')):
            with self.subTest(flag=flag, value=value):
                result = subprocess.run(['bash', str(script), flag, value],
                                        capture_output=True, text=True)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertNotIn('build the test data', result.stderr)



class BoundedEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.meta = {'workload': 4, 'slots': 10, 'seed': 0, 'shape_vus': 10}
        self.records = []
        self.responses = []
        for i in range(10):
            key = json.dumps([i, 'home:image', 0, 'GET', '/Items/a/Images/Primary', {}])
            self.records.append({'selection': i, 'iteration': i, 'screen': 'home', 'keys': [key], 'ok': True})
            self.responses.append({'shape': 'home:image', 'slot': i, 'key': key,
                                   'status': 200, 'content_type': 'image/jpeg', 'bytes': 100})
        self.summary = {**self.meta, 'iterations': 10, 'requests': 10, 'elapsed_ms': 20}
        (self.root/'shape-summary.json').write_text(json.dumps(self.summary))
        self.save()

    def save(self):
        self.log(self.root/'shape.log', self.records + self.responses)

    def log(self, path, records):
        path.write_text('\n'.join(json.dumps({'msg': json.dumps(r)}) for r in records))

    def evidence(self):
        return report.shape_evidence(self.root, self.meta, report.load_shape(self.root))

    def test_shape_slots_are_complete_once_across_vus(self):
        self.records.reverse()  # completion order across VUs is irrelevant
        self.responses.reverse()
        self.save()
        problem, index, coverage = self.evidence()
        self.assertIsNone(problem)
        self.assertEqual(set(index), set(range(10)))
        self.assertEqual(coverage['distinct_requests'], 1)

    def test_missing_or_duplicated_slot_fails(self):
        self.records[-1] = self.records[0]
        self.save()
        self.assertIn('missing or duplicated', self.evidence()[0])

    def test_missing_image_response_fails_coverage(self):
        self.responses.pop()
        self.save()
        self.assertIn('request evidence', self.evidence()[0])

    def test_changed_seed_fails(self):
        self.meta['seed'] = 9
        self.assertIn('seed', self.evidence()[0])

    def test_601st_iteration_wraps_to_validated_slot(self):
        # Ten-slot fixture exercises the same modulo rule cheaply over 601 iterations.
        _, expected, _ = self.evidence()
        measured = [{**copy.deepcopy(expected[i % 10]), 'iteration': i} for i in range(601)]
        path = self.root/'selections.log'
        self.log(path, measured)
        summary = {'iterations': 601, 'requests': 601}
        self.assertEqual(report.selection_problems(path, summary, expected, 10), {})
        measured[-1]['keys'] = []
        self.log(path, measured)
        summary['requests'] -= 1
        self.assertIn('selections differ', report.selection_problems(path, summary, expected, 10)['home'])

    def test_missing_timed_evidence_cannot_pass(self):
        _, expected, _ = self.evidence()
        problems = report.selection_problems(self.root/'absent.log', self.summary, expected, 10)
        self.assertIn('incomplete', problems['home'])

    def test_measured_seed_must_match_shape_seed(self):
        _, expected, _ = self.evidence()
        path = self.root/'selections.log'
        self.log(path, self.records)
        problems = report.selection_problems(path, {**self.summary, 'seed': 99}, expected, 10, self.meta)
        self.assertIn('seed', problems['home'])

    def test_failed_dependency_fails_screen(self):
        _, expected, _ = self.evidence()
        self.records[0]['ok'] = False
        path = self.root/'selections.log'
        self.log(path, self.records)
        self.assertIn('dependent request', report.selection_problems(path, self.summary, expected, 10)['home'])

    def test_image_bytes_are_diagnostic_but_type_and_empty_body_fail(self):
        original = report.load_shape(self.root)
        self.responses[0]['bytes'] = 200
        self.save()
        self.assertIsNone(report.comparable(report.load_shape(self.root), original, ['home:image']))
        self.responses[0]['content_type'] = 'text/html'
        self.save()
        self.assertIn('content_type', report.comparable(report.load_shape(self.root), original, ['home:image']))
        self.responses[0]['content_type'] = 'image/jpeg'
        self.responses[0]['bytes'] = 0
        self.save()
        self.assertIn('empty', report.comparable(report.load_shape(self.root), original, ['home:image']))
        self.assertIn('empty', report.oracle_failed(report.load_shape(self.root), ['home:image']))

    def test_request_key_includes_each_image_occurrence(self):
        original = report.load_shape(self.root)
        self.responses.pop()
        self.save()
        self.assertIn('picks differ', report.comparable(report.load_shape(self.root), original, ['home:image']))

    def test_both_servers_invalid_json_is_still_a_failure(self):
        self.log(self.root/'shape.log', [{'shape': 'home:items', 'key': 'key', 'status': 200,
                                        'content_type': 'application/json', 'invalid_json': True}])
        shape = report.load_shape(self.root)
        self.assertIsNotNone(report.comparable(shape, shape, ['home:items']))

    def test_ordered_ids_and_per_item_types_preserve_duplicates(self):
        response = {'shape': 'movies:items', 'key': 'pick', 'status': 200, 'count': 2,
                    'ids': ['a', 'a'], 'types': {'$.Items[0].Name': 'string', '$.Items[1].Name': 'string'}}
        self.log(self.root/'shape.log', [response])
        original = report.load_shape(self.root)
        response['ids'] = ['a', 'b']
        self.log(self.root/'shape.log', [response])
        self.assertIn('ids differs', report.comparable(report.load_shape(self.root), original, ['movies:items']))
        response['ids'] = ['a', 'a']
        response['types']['$.Items[1].Name'] = 'null'
        self.log(self.root/'shape.log', [response])
        self.assertIn('per-item', report.comparable(report.load_shape(self.root), original, ['movies:items']))

    def test_pool_export_uses_source_ids_and_fixed_cutoff(self):
        api = Mock()
        api.get.side_effect = [{'Items': [{'Id': 'm', 'Name': 'The Shared Movie'}], 'TotalRecordCount': 101},
                               {'Items': [{'Id': 's'}]}]
        pools = seed.export_pools(api, {'user': 'u', 'movies_view': 'm', 'shows_view': 's'})
        self.assertEqual(pools['movies'], ['m'])
        self.assertEqual(pools['series'], ['s'])
        self.assertEqual(pools['movieCount'], 101)
        self.assertEqual(pools['terms'], ['Movie', 'Shared'])
        self.assertEqual(pools['nextUpCutoff'], '2025-09-01T00:00:00.000Z')



class InstrumentTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def resource(self, times=(0, 0.1, 0.2, 0.3, 0.4), counter=True):
        (self.root/'windows.json').write_text(json.dumps({'loaded': {'start': .05, 'end': .35}}))
        (self.root/'mem.csv').write_text('t,anon' + (',cpu_usec' if counter else '') + '\n' +
            '\n'.join(f'{t},100' + (f',{int(t*2000000)}' if counter else '') for t in times))
        return report.resource_windows(self.root, {'resource_schema': 1, 'mem_sample_ms': 100})['loaded']

    def test_cpu_uses_raw_counter_at_documented_brackets(self):
        row = self.resource()
        self.assertNotIn('error', row)
        self.assertAlmostEqual(row['cpu_seconds'], .8)
        self.assertEqual(row['observed_start'], 0)
        self.assertEqual(row['observed_end'], .4)
        self.assertEqual(row['samples'], 5)

    def test_started_window_without_boundaries_fails_resources(self):
        (self.root/'phases.json').write_text(json.dumps({'loaded':{'status':'completed','start':1}}))
        self.assertIn('missing resource window',report.resource_windows(self.root, {'resource_schema':1})['loaded']['error'])

    def test_stopped_sampler_cannot_cover_window(self):
        self.assertIn('error', self.resource(times=(0, .1, .2)))

    def test_large_gap_and_missing_counter_fail(self):
        self.assertIn('bracket', self.resource(times=(0, .1, 1))['error'])
        self.assertIn('counter', self.resource(counter=False)['error'])

    def test_internal_sample_gap_fails_even_with_close_brackets(self):
        self.resource(times=(0, .1, .9, 1))
        (self.root/'windows.json').write_text(json.dumps({'loaded':{'start':.05,'end':.95}}))
        self.assertIn('gap',report.resource_windows(self.root, {'resource_schema':1})['loaded']['error'])

    def test_raw_cpu_counter_cannot_go_backwards(self):
        self.resource()
        (self.root/'mem.csv').write_text('t,anon,cpu_usec\n0,100,800\n0.1,100,600\n0.4,100,900\n')
        row = report.resource_windows(self.root, {'resource_schema': 1})['loaded']
        self.assertIn('decreased', row['error'])

    def test_smt_shared_physical_core_is_rejected(self):
        for cpu, siblings in ((0, '0,2'), (2, '0,2'), (1, '1,3'), (3, '1,3')):
            p=self.root/f'cpu{cpu}/topology'
            p.mkdir(parents=True)
            (p/'thread_siblings_list').write_text(siblings)
        with self.assertRaisesRegex(ValueError, 'physical cores'):
            mem_sample.topology('0', '2', self.root)
        with self.assertRaisesRegex(ValueError, 'overlap'):
            mem_sample.topology('0', '0', self.root)
        self.assertEqual(mem_sample.topology('0','1',self.root)['checked_cpus'], [0,1,2,3])

    def test_range_validation_rejects_ignored_truncated_or_wrong_range(self):
        ttfs.validate_range(206, 'bytes 0-1048575/2000000', 1048576)
        for status, header, count in ((200,'bytes 0-1048575/2000000',1048576),
                (206,'bytes 0-1048575/2000000',10), (206,'bytes 1-1048576/2000000',1048576),
                (206,None,1048576), (206,'bytes 0-1048575/1',1048576)):
            with self.subTest(status=status,header=header,count=count), self.assertRaises(ValueError):
                ttfs.validate_range(status, header, count)

    @patch.object(ttfs.subprocess, 'check_output')
    def test_segment_probe_requires_streams_and_positive_duration(self, probe):
        probe.return_value = json.dumps({'streams':[{'codec_type':'video','codec_name':'h264'},
            {'codec_type':'audio','codec_name':'aac'}], 'format':{'duration':'3.2','format_name':'mpegts'}}).encode()
        self.assertEqual(ttfs.probe_segment(b'body')['duration_s'],3.2)
        probe.return_value = b'{"streams": [], "format":{"duration":"0"}}'
        with self.assertRaisesRegex(ValueError,'duration'):
            ttfs.probe_segment(b'body')
        with self.assertRaisesRegex(ValueError,'empty'):
            ttfs.probe_segment(b'')

    def test_eof_before_first_byte_has_no_successful_ttfb(self):
        import io
        class Response(io.BytesIO):
            status=206
            headers={'Content-Range':'bytes 0-1048575/2000000'}
        ids=self.root/'ids.json'
        ids.write_text(json.dumps({'stream':'a','stream_source':'b','user':'u','token':'test'}))
        out=self.root/'ttfs.json'
        def response(req, **kwargs):
            return Response(b'{"MediaSources":[{}]}' if req.method=='POST' else b'')
        with (patch.object(ttfs.urllib.request, 'urlopen', side_effect=response),
              patch.object(ttfs.time, 'sleep'), patch('builtins.print'),
              patch.object(ttfs.sys, 'argv', ['ttfs.py','http://localhost',str(ids),str(out),'1'])):
            self.assertEqual(ttfs.main(),1)
        result=json.loads(out.read_text())
        self.assertIn('EOF',result['direct'][0]['error'])
        self.assertNotIn('ttfb_ms',result['direct'][0])

    def test_start_failure_stops_and_joins_poller(self):
        import threading
        finished=threading.Event()
        def command(args, **kwargs):
            if args[1]=='start':
                raise subprocess.TimeoutExpired(args,1)
        def poll(url, headers, timeout_s, poll_s, stop, started):
            started.set()
            stop.wait(2)
            finished.set()
        with (patch.object(coldstart.subprocess, 'run', side_effect=command),
              patch.object(coldstart, 'poll', side_effect=poll)):
            with self.assertRaises(subprocess.TimeoutExpired):
                coldstart.restart('owned','http://localhost',{},.01)
        self.assertTrue(finished.is_set())

    def test_restart_poller_is_running_before_start_and_joined(self):
        import threading
        events=[]
        release=threading.Event()
        def command(args, **kwargs):
            events.append(args[1])
            if args[1]=='start':
                self.assertIn('poll',events)
                release.set()
        def poll(url, headers, timeout_s, poll_s, stop, started):
            events.append('poll'); started.set()
            release.wait(1)
            return 103.2
        with patch.object(coldstart.subprocess,'run',side_effect=command), \
             patch.object(coldstart,'poll',side_effect=poll), \
             patch.object(coldstart,'started_at',return_value=100):
            result=coldstart.restart('owned','http://localhost',{},.01)
        self.assertEqual(events,['stop','poll','start'])
        self.assertAlmostEqual(result['home_ms'],3200)


if __name__ == '__main__':
    unittest.main()
