"""Reporter correctness only: python3 -m unittest discover -s bench -p test_report.py."""
import copy
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch
import urllib.error

import gate
import report
import coldstart
import mem_sample
import scan
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
            tag = {'jellyfin': 'jellyfin/jellyfin:12.2',
                   'ferrofin': 'ferrofin:bench'}[server]
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

    def test_report_evidence_is_tabular_and_preserves_failures_and_missing_values(self):
        model = self.build(self.a, self.b)
        for server, _ in model['servers']:
            model['phases'][server] = [
                {'loaded': {'status': 'failed', 'reason': '<timeout>|retry', 'start': 1000, 'end': 1002}}, None]
            model['coverage'][server] = [
                {'slots': 600, 'requests': 8732, 'distinct_requests': 2913,
                 'elapsed_s': 32.75, 'distinct_picks': {'detail:item': 120}},
                {'unavailable': 'legacy workload'}]
            model['levels']['loaded']['image_bytes'][server] = [0, None]
        for rendered in (report.render_md(model), report.render_html(model)):
            for expected in ('Phase outcomes', 'Shape coverage', 'Distinct requests',
                             'detail:item: 120', 'legacy workload', 'no phase evidence',
                             '2.000', 'Image response bytes', 'unavailable'):
                self.assertIn(expected, rendered)
            for serialized_key in ('"distinct_picks"', '"status"', '"elapsed_s"', "{'movies':"):
                self.assertNotIn(serialized_key, rendered)
            self.assertNotIn('<pre>', rendered)
        self.assertIn('&lt;timeout&gt;|retry', report.render_html(model))
        self.assertIn('<timeout>\\|retry', report.render_md(model))
        self.assertIn('| 1 | Ferrofin | 0 |', report.image_table(model, 'loaded'))

    def test_cpu_tables_compare_servers_and_preserve_every_repetition(self):
        model = self.build(self.a, self.b)
        for n, (server, _) in enumerate(model['servers']):
            model['resources'][server] = [
                {'loaded': {'cpu_seconds': n + i + .25, 'samples': 1202,
                            'observed_start': 1000, 'observed_end': 1120,
                            'max_gap_s': .101}}
                for i in range(2)]
        rows = dict(report.cpu_tables(model)[0][1])
        self.assertEqual(rows['loaded · run 1']['jellyfin'], '0.250')
        self.assertEqual(rows['loaded · run 2']['ferrofin'], '2.250')
        for rendered in (report.render_md(model), report.render_html(model)):
            section = rendered.split('CPU and sample coverage — invocation and idle windows', 1)[1].split('Phase outcomes', 1)[0]
            for expected in ('CPU consumption (CPU-seconds)', 'loaded · run 2', '2.250',
                             'Samples', '1202', '101.000', '120.000', 'Observed start (UTC)', '12.2'):
                self.assertIn(expected, section)
            self.assertNotIn('cpu_seconds', section)
            self.assertNotIn('<pre>', section)

    def test_cpu_utilization_and_completed_screen_cost(self):
        model = self.build()
        model['meta']['server_cpus'] = '8-15'
        for server, _ in model['servers']:
            model['resources'][server] = [{'loaded': {'cpu_seconds': 24,
                'observed_start': 1000, 'observed_end': 1120, 'samples': 1202}}]
            path = Path(model['runs'][0]) / server / 'k6-loaded.json'
            summary = json.loads(path.read_text())
            summary['iterations'] = 600
            self.write(path, summary)
        tables = {title: dict(rows) for title, rows in report.cpu_tables(model)}
        for server, _ in model['servers']:
            self.assertEqual(tables['Average CPU cores busy']['loaded · run 1'][server], '0.200')
            self.assertEqual(tables['Allocated CPU capacity busy (%)']['loaded · run 1'][server], '2.50%')
            self.assertEqual(tables['CPU milliseconds per completed screen']['loaded · run 1'][server], '40.000')
        model['resources']['ferrofin'][0]['loaded']['cpu_seconds'] = 12
        rows = dict(dict(report.cpu_tables(model))['CPU per screen comparison (validated workloads only)'])
        key = next(k for k in rows if 'vs Jellyfin 12' in k)
        self.assertEqual(rows[key]['ferrofin'], '50.0% less CPU per screen')
        model['levels']['loaded']['screens'][0][1]['ferrofin'].flag = 'different work'
        rows = dict(dict(report.cpu_tables(model))['CPU per screen comparison (validated workloads only)'])
        self.assertEqual(rows[key]['ferrofin'], 'withheld — validation flags')

    def test_cpu_table_retains_failed_sampler_and_escapes_reason(self):
        model = self.build()
        model['resources']['ferrofin'] = [{'loaded': {'cpu_seconds': 0.0, 'samples': 2}}]
        model['phases']['ferrofin'] = [{'sampler': {'status': 'failed', 'reason': '<lost>|exit'}}]
        rows = dict(report.cpu_tables(model)[0][1])
        self.assertEqual(rows['loaded · run 1']['ferrofin'], '0.000 ⚠')
        self.assertIn('&lt;lost&gt;|exit', report.render_html(model))
        self.assertIn('<lost>\\|exit', report.render_md(model))

    def test_cpu_table_does_not_invent_missing_historical_counters(self):
        model = self.build()
        for server, _ in model['servers']:
            model['resources'][server] = [{'loaded': {'cpu_seconds': None, 'samples': 2}}]
        rows = dict(report.cpu_tables(model)[0][1])
        self.assertEqual(set(rows['loaded · run 1'].values()), {'unavailable'})
        self.assertIn('unavailable — no cumulative CPU counters', report.render_html(model))

    def test_clean_report_and_optional_servers(self):
        m = self.build(self.a, self.b)
        self.assertIsNone(self.home(m).flag)
        self.assertIn('faster on 7', self.readme(m))
        self.assertIn('3 logical CPUs', self.readme(m))
        self.assertIn('12.2', report.render_md(m))
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
        self.edit(self.b / 'jellyfin/k6-loaded.json', lambda x: x['endpoints']['home:items'].update(ok=0))
        m = self.build(self.a, self.b)
        self.assertIn('failed', self.home(m).flag)
        self.assertIn('failed', self.home(m, 'jellyfin').flag)

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
                           ('server_cpus', '4-7'), ('ids_sha256', 'changed'), ('screens_sha256', 'changed'), ('background_policy', 1)]:
            with self.subTest(key=key):
                self.write(self.b / 'run.json', {**original, key: value})
                with self.assertRaisesRegex(ValueError, key):
                    self.build(self.a, self.b)
        self.write(self.b / 'run.json', original)
        (self.b / 'jellyfin/image.txt').write_text('jellyfin/jellyfin:12.0 sha256:different')
        with self.assertRaisesRegex(ValueError, 'jellyfin image'):
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

    def ferrofin_only(self, name, p50=10, oracle=False):
        run = self.make_run(name)
        if not oracle:
            shutil.rmtree(run / 'jellyfin')
        self.edit(run / 'ferrofin/k6-loaded.json', lambda x: [v.update(p50=p50) for group in ('screens', 'endpoints') for v in x[group].values()])
        return run

    def test_ferrofin_only_candidate_colours_delta_against_ferrofin_only_baseline(self):
        model, base = self.build(self.ferrofin_only('cand', p50=20)), self.build(self.ferrofin_only('base'))
        self.assertIn('no oracle run', self.home(model).flag)  # the oracle ratio stays caveated
        self.assertIsNone(self.home(model).own_flag)
        delta = report.delta_html(self.home(model), self.home(base), report.load_gate())
        self.assertIn("class='delta bad'", delta)
        self.assertIn('+100%', delta)
        page = report.render_html(model, base)
        self.assertIn("delta bad", page)
        self.assertNotIn("delta provisional", page)

    def test_oracle_problems_leave_the_delta_alone_and_own_problems_caveat_it(self):
        cand, base = self.ferrofin_only('cand', p50=20, oracle=True), self.ferrofin_only('base', oracle=True)
        self.edit(cand / 'jellyfin/k6-loaded.json', lambda x: x['endpoints']['home:items'].update(ok=0))
        self.shapes(cand / 'jellyfin', [{'shape': n, 'url': f'http://jellyfin/{n}', 'status': 200,
                                          'count': 9, 'fields': ['.Items'], 'bytes': 100}
                                         for n in [f'{s}:items' for s in report.SCREENS] + ['image']])
        model, base_model = self.build(cand), self.build(base)
        self.assertIn('failed', self.home(model).flag)
        self.assertIsNone(self.home(model).own_flag)
        self.assertIn('delta bad', report.delta_html(self.home(model), self.home(base_model), report.load_gate()))
        self.edit(cand / 'ferrofin/k6-loaded.json', lambda x: x['screens']['home'].update(ok=0.5))
        delta = report.delta_html(self.home(self.build(cand)), self.home(base_model), report.load_gate())
        self.assertIn('delta provisional', delta)
        self.assertIn('measured requests failed', delta)

    def test_malformed_gate_file_is_a_readable_error(self):
        for name, text in [('missing', None), ('not toml', 'screens = ['), ('no absolute', '[screens]\nrelative_pct = 15\n')]:
            with self.subTest(name):
                path = self.root / f'{name}.toml'
                if text is not None:
                    path.write_text(text)
                with self.assertRaisesRegex(ValueError, 'gate thresholds unreadable'):
                    report.load_gate(str(path))

    def test_picker_selects_several_baseline_runs(self):
        picker = report.picker_html([('a', 'a ·'), ('b', 'b ·'), ('c', 'c ·')], ['a'], ['b', 'c'])
        self.assertIn("name='baseline' value='b' checked", picker)
        self.assertIn("name='baseline' value='c' checked", picker)
        self.assertNotIn("name='baseline' value='a' checked", picker)

    def test_loaded_only_runs_without_a_shape_pass_still_colour(self):
        cand, base = self.ferrofin_only('cand', p50=20), self.ferrofin_only('base')
        for run in (cand, base):
            (run / 'ferrofin/shape.log').unlink()
        model, base = self.build(cand), self.build(base)
        self.assertIn('delta bad', report.delta_html(self.home(model), self.home(base), report.load_gate()))

    def test_delta_is_provisional_when_the_server_did_different_work(self):
        model, base = self.ferrofin_only('cand', p50=20), self.ferrofin_only('base')
        self.shapes(base / 'ferrofin', [{'shape': n, 'url': f'http://ferrofin/{n}', 'status': 200,
                                          'count': 2 if n == 'home:items' else 1, 'fields': ['.Items'], 'bytes': 100}
                                         for n in [f'{s}:items' for s in report.SCREENS] + ['image']])
        model, base = self.build(model), self.build(base)
        delta = report.delta_html(self.home(model), self.home(base), report.load_gate())
        self.assertIn('delta provisional', delta)
        self.assertIn('responses differ', delta)
        movies = lambda m: dict(m['levels']['loaded']['screens'])['movies']['ferrofin']
        self.assertIn('delta bad', report.delta_html(movies(model), movies(base), report.load_gate()))

    def test_baseline_of_three_runs_is_compared_by_its_median(self):
        # runs[0] alone would read as a 75% improvement; the median says nothing moved.
        runs = [self.ferrofin_only('base1', p50=25), self.ferrofin_only('base2'), self.ferrofin_only('base3')]
        model, base = self.build(self.ferrofin_only('cand')), self.build(*runs)
        delta = report.delta_html(self.home(model), self.home(base), report.load_gate())
        self.assertIn('+0%', delta)
        self.assertIn('median of 3 runs', delta)
        self.assertIn("class='delta '", delta)
        page = report.render_html(model, base)
        self.assertIn('1 run: cand', page)
        self.assertIn('baseline, median of 3 runs: base1, base2, base3', page)

    def test_delta_inside_the_noise_band_stays_uncoloured(self):
        cell = lambda v, gate, key=None: report.gated(report.Cell([v], report.sig3), gate, key)
        gate = report.load_gate()
        for name, now, then, kind in [('relative under 15%', 11, 10, 'screens'),
                                      ('absolute under 0.5 ms', 1.4, 1, 'endpoints'),
                                      ('memory under 25%', 120, 100, 'memory'),
                                      ('no gate row', 200, 100, None)]:
            with self.subTest(name):
                self.assertIn("class='delta '", report.delta_html(cell(now, kind), cell(then, kind), gate))
        self.assertIn('delta bad', report.delta_html(cell(1.6, 'endpoints'), cell(1, 'endpoints'), gate))
        self.assertIn('delta good', report.delta_html(cell(50, 'memory'), cell(100, 'memory'), gate))
        noisy = {**gate, 'cells': {'loaded/screens/detail': {'relative_pct': 80, 'absolute': 5}}}
        self.assertIn("class='delta '", report.delta_html(cell(17, 'screens', 'loaded/screens/detail'), cell(10, 'screens'), noisy))

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
        for server, counts in [('jellyfin', [1, 2]), ('ferrofin', [2, 1])]:
            self.shapes(self.a / server, [{'shape': 'movies:items', 'url': f'http://{server}/Items?page={i}',
                                         'status': 200, 'count': count, 'fields': ['.Items']}
                                        for i, count in enumerate(counts)])
        self.assertIsNotNone(report.comparable(report.load_shape(self.a / 'ferrofin'),
                                              report.load_shape(self.a / 'jellyfin'), ['movies:items']))

    def test_image_byte_sizes_do_not_flag_cells(self):
        for server, size in [('jellyfin', 100), ('ferrofin', 200)]:
            path = self.a / server / 'shape.log'
            records = [json.loads(json.loads(line)['msg']) for line in path.read_text().splitlines()]
            for r in records:
                if r['shape'] == 'image':
                    r['bytes'] = size
            self.shapes(path.parent, records)
        m = self.build()
        self.assertIsNone(dict(m['levels']['loaded']['endpoints'])['image']['ferrofin'].flag)
        self.assertIn('faster', self.row(self.readme(m)))

    def test_inventory_differences_remain_diagnostic_across_repetitions(self):
        self.edit(self.b / 'ferrofin/counts.json', lambda x: x.update(movies=1, persons=5000))
        self.edit(self.b / 'jellyfin/counts.json', lambda x: x.update(persons=5158))
        m = self.build(self.a, self.b)
        for kind in ('screens', 'endpoints'):
            for _, cells in m['levels']['loaded'][kind]:
                self.assertIsNone(cells['ferrofin'].flag)
        self.assertTrue(any('count movies' in x for x in m['work']['ferrofin']))
        self.assertTrue(any('count persons' in x for x in m['work']['ferrofin']))
        self.assertIn('faster', self.row(self.readme(m)))

    def test_missing_inventory_stays_visible_without_flagging_latency(self):
        for server in ('ferrofin', 'jellyfin'):
            (self.b / server / 'counts.json').unlink()
        m = self.build(self.a, self.b)
        self.assertIsNone(self.home(m).flag)
        self.assertIsNone(self.home(m, 'jellyfin').flag)
        self.assertIn('missing counts.json', m['work']['ferrofin'])

    def test_actual_response_counts_still_flag_affected_latency(self):
        path = self.b / 'ferrofin/shape.log'
        records = [json.loads(json.loads(line)['msg']) for line in path.read_text().splitlines()]
        next(r for r in records if r['shape'] == 'home:items')['count'] = 2
        self.shapes(path.parent, records)
        m = self.build(self.a, self.b)
        self.assertIsNotNone(self.home(m).flag)
        self.assertIsNone(dict(m['levels']['loaded']['screens'])['movies']['ferrofin'].flag)
        self.assertIn('— ⚠', self.row(self.readme(m)))

    def test_transport_failure_in_oracle_shape(self):
        self.shapes(self.a / 'jellyfin', [{'shape': 'home:items', 'url': '/home:items', 'status': 0}])
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

    def test_single_server_render(self):
        shutil.rmtree(self.a / 'jellyfin')
        text = self.readme()
        self.assertIn('no Jellyfin reference selected', text)
        self.assertNotIn('faster', self.row(text))

    def test_dropped_work_withholds_resource_comparisons(self):
        self.edit(self.a / 'ferrofin/k6-loaded.json', lambda x: x.update({'dropped_iterations': 1}))
        m = self.build()
        self.assertIsNotNone(self.home(m).flag)
        for name, cells in m['memory']:
            if name in report.README_MEMORY:
                self.assertIsNotNone(cells['ferrofin'].flag)

    def test_missing_window_is_incomplete_not_zero_errors(self):
        (self.b / 'ferrofin/k6-loaded.json').unlink()
        m = self.build(self.a, self.b)
        self.assertIn('missing load window', self.home(m).flag)
        self.assertNotIn('No measured request failed', self.readme(m))

    def test_later_oracle_timing_failure_withholds_ratio(self):
        self.edit(self.b / 'jellyfin/coldstart.json', lambda x: x['runs'][0].update(home_ms=None))
        text = self.readme(self.build(self.a, self.b))
        line = next(x for x in text.splitlines() if x.startswith('| cold start'))
        self.assertIn('— ⚠', line)
        self.assertNotIn('faster', line)

    def test_empty_image_is_not_accepted(self):
        for server in ('ferrofin', 'jellyfin'):
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
        self.write(self.b / 'jellyfin/system-info.json', {'Version': '12.2.0'})
        new = self.build(self.b)
        for render in (report.render_md, report.render_html,
                       lambda m: report.render_readme(m, m['runs'])):
            with self.subTest(renderer=render):
                self.assertIn('Jellyfin 12.2.0', render(new))
                self.assertNotIn('Jellyfin 12.2.0', render(old))

    def test_digest_without_version_is_not_labelled_stable(self):
        d = self.a / 'jellyfin'
        (d / 'image.txt').write_text('jellyfin/jellyfin@sha256:abc sha256:def')
        self.assertEqual(report.server_label(d, 'jellyfin'), 'Jellyfin (version unknown)')

    def test_reported_version_mismatch_refuses_aggregation(self):
        self.write(self.a / 'jellyfin/system-info.json', {'Version': '12.2.0'})
        with self.assertRaisesRegex(ValueError, 'jellyfin reported version'):
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
        for field in ('workload', 'ids_sha256', 'screens_sha256', 'seed', 'warmup_seed', 'slots', 'pools', 'user_isolation', 'actors'):
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

    def test_invalid_shape_vus_fail_before_fixture_or_docker_access(self):
        script = Path(__file__).parent / 'run.sh'
        for value in ('0', '-1', '1.5', 'no'):
            with self.subTest(value=value):
                result = subprocess.run(['bash', str(script), '--only', 'shape'],
                                        env=dict(os.environ, SHAPE_VUS=value), capture_output=True, text=True)
                self.assertEqual(result.returncode, 2)
                self.assertIn('SHAPE_VUS must be a positive integer', result.stderr)

    def test_invalid_selections_fail_before_fixture_or_docker_access(self):
        script = Path(__file__).parent / 'run.sh'
        for flag, value in (('--only', ''), ('--only', 'loaded,bogus'), ('--only', 'loaded,,shape'),
                            ('--only', 'loaded shape'), ('--only', ',counts'),
                            ('--servers', ''), ('--servers', 'jellyfin,unknown')):
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

    def test_isolated_shape_and_measurement_records_keep_their_actor(self):
        self.meta.update(user_isolation=1, actors={'browse': 'a', 'playback': 'b'})
        self.summary['user_isolation'] = 1
        (self.root/'shape-summary.json').write_text(json.dumps(self.summary))
        for record in self.records:
            record.update(actor='browse', user='a')
        self.save()
        error, index, _ = self.evidence()
        self.assertIsNone(error)
        measured = copy.deepcopy(self.records)
        measured[0]['actor'] = 'playback'
        self.log(self.root/'measured.log', measured)
        problems = report.selection_problems(self.root/'measured.log', self.summary, index, 10, self.meta)
        self.assertIn('actor', problems['home'])
        self.records[0]['user'] = 'b'
        self.save()
        self.assertIn('actor', self.evidence()[0])

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



class UserIsolationTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.ids = {'user': 'a', 'viewer': 'b', 'user_name': 'bench', 'password': 'bench',
                    'viewer_name': 'viewer', 'viewer_password': 'viewer', 'token': 'control',
                    'pools': {'movies': ['movie']}}

    def test_roles_require_distinct_users_and_explicit_credentials(self):
        seed.validate_user_roles(self.ids)
        for update in ({'viewer': 'A'}, {'viewer_password': ''}, {'user_name': None}):
            with self.subTest(update=update), self.assertRaises(RuntimeError):
                seed.validate_user_roles({**self.ids, **update})

    def test_session_owner_and_additional_users_are_checked(self):
        sessions = [{'Client': 'bench', 'DeviceId': 'bench-browse', 'UserId': 'a', 'AdditionalUsers': []},
                    {'Client': 'bench', 'DeviceId': 'bench-playback', 'UserId': 'b', 'AdditionalUsers': []}]
        seed.check_role_sessions(sessions, self.ids)
        for change in ({'UserId': 'a'}, {'AdditionalUsers': [{'UserId': 'a'}]}, {'DeviceId': 'other'}):
            with self.subTest(change=change), self.assertRaises(RuntimeError):
                seed.check_role_sessions([sessions[0], {**sessions[1], **change}], self.ids)
        with self.assertRaises(RuntimeError):
            seed.check_role_sessions(sessions + [sessions[1]], self.ids)

    def test_harness_sessions_are_accounted_for(self):
        roles = [{'Client': 'bench', 'DeviceId': 'bench-browse', 'UserId': 'a', 'AdditionalUsers': []},
                 {'Client': 'bench', 'DeviceId': 'bench-playback', 'UserId': 'b', 'AdditionalUsers': []}]
        cold = {'Client': 'bench', 'DeviceId': coldstart.DEVICE, 'UserId': 'a', 'AdditionalUsers': []}
        self.assertIn(coldstart.DEVICE, seed.CONTROL_DEVICES)
        self.assertIn(ttfs.DEVICE, seed.CONTROL_DEVICES)
        seed.check_role_sessions(roles + [cold, {**cold, 'DeviceId': ttfs.DEVICE}], self.ids)
        seed.check_role_sessions(roles + [{**cold, 'Client': 'other', 'DeviceId': 'x', 'UserId': 'z'}], self.ids)
        for change in ({'DeviceId': 'bench-unknown'}, {'UserId': 'b'}, {'AdditionalUsers': [{'UserId': 'b'}]}):
            with self.subTest(change=change), self.assertRaises(RuntimeError):
                seed.check_role_sessions(roles + [{**cold, **change}], self.ids)

    def test_user_state_requires_exact_item_coverage(self):
        api = Mock()
        api.get.return_value = {'Items': [{'Id': 'a', 'UserData': {'PlayCount': 0}}]}
        self.assertEqual(seed.movie_user_state(api, 'user', ['a'])['a']['PlayCount'], 0)
        for rows in ([], [{'Id': 'x', 'UserData': {'PlayCount': 0}}],
                     [{'Id': 'a', 'UserData': {}}], [{'Id': 'a', 'UserData': {'PlayCount': 0}}] * 2):
            api.get.return_value = {'Items': rows}
            with self.subTest(rows=rows), self.assertRaises(RuntimeError):
                seed.movie_user_state(api, 'user', ['a'])

    def test_user_state_requests_are_bounded(self):
        api = Mock()
        api.get.side_effect = lambda path, **q: {'Items': [{'Id': i, 'UserData': {'PlayCount': 0}}
                                                        for i in q['Ids'].split(',')]}
        result = seed.movie_user_state(api, 'user', [str(i) for i in range(201)])
        self.assertEqual(len(result), 201)
        self.assertEqual([c.kwargs['Limit'] for c in api.get.call_args_list], [100, 100, 1])

    def test_state_drift_is_detected_without_resetting_it(self):
        before = {'browse_user': 'a', 'playback_user': 'b', 'user_state': {'movie': {'PlayCount': 0}},
                  'home': {'resume': ['movie']}, 'home_images': ['movie']}
        self.assertEqual(seed.state_changes(before, before), {'changed_items': [], 'home_changed': False})
        after = copy.deepcopy(before)
        after['user_state']['movie']['PlayCount'] = 1
        after['home_images'] = []
        self.assertEqual(seed.state_changes(before, after), {'changed_items': ['movie'], 'home_changed': True})
        with self.assertRaises(RuntimeError):
            seed.state_changes(before, {**before, 'user_state': {}})

    def test_authentication_uses_role_credentials_and_private_output(self):
        source, output = self.root/'source.json', self.root/'private.json'
        source.write_text(json.dumps(self.ids))
        control, browse, playback = Mock(), Mock(), Mock()
        control.get.return_value = [{'Client': 'bench', 'DeviceId': 'bench-' + r, 'UserId': u}
                                    for r, u in (('browse', 'a'), ('playback', 'b'))]
        browse.post.return_value = {'User': {'Id': 'a', 'Policy': {}}, 'AccessToken': 'browse-secret'}
        playback.post.return_value = {'User': {'Id': 'b', 'Policy': {}}, 'AccessToken': 'playback-secret'}
        with patch.object(seed, 'role_api', side_effect=[control, browse, playback]) as api, \
             patch.object(seed, 'movie_user_state'):
            seed.authenticate_roles('http://server', source, output)
        self.assertEqual([c.args[2] for c in api.call_args_list], ['bench-state', 'bench-browse', 'bench-playback'])
        playback.post.assert_called_once_with('/Users/AuthenticateByName', {'Username': 'viewer', 'Pw': 'viewer'})
        saved = json.loads(output.read_text())
        self.assertEqual(saved['browse_token'], 'browse-secret')
        self.assertEqual(saved['playback_token'], 'playback-secret')
        self.assertEqual(output.stat().st_mode & 0o777, 0o600)
        self.assertNotIn('secret', (self.root/'user-roles.json').read_text())
        self.assertEqual(json.loads(source.read_text()), self.ids)

    def test_wrong_identity_fails_without_browse_token_fallback(self):
        source, output = self.root/'source.json', self.root/'private.json'
        source.write_text(json.dumps(self.ids))
        api = Mock()
        api.post.return_value = {'User': {'Id': 'wrong'}, 'AccessToken': 'secret'}
        with patch.object(seed, 'role_api', return_value=api), self.assertRaisesRegex(RuntimeError, 'wrong identity'):
            seed.authenticate_roles('http://server', source, output)
        self.assertFalse(output.exists())

    def test_rejected_credentials_fail_without_writing_role_tokens(self):
        source, output = self.root/'source.json', self.root/'private.json'
        source.write_text(json.dumps(self.ids))
        api = Mock()
        api.post.side_effect = urllib.error.HTTPError('http://server', 401, 'unauthorized', {}, None)
        self.addCleanup(api.post.side_effect.close)
        with patch.object(seed, 'role_api', return_value=api), self.assertRaises(urllib.error.HTTPError):
            seed.authenticate_roles('http://server', source, output)
        self.assertFalse(output.exists())
        api.post.assert_called_once()

    def test_actor_evidence_requires_both_role_and_user(self):
        meta = {'user_isolation': 1, 'actors': {'browse': 'a', 'playback': 'b'}}
        good = {'screen': 'playback', 'actor': 'playback', 'user': 'b'}
        self.assertIsNone(report.actor_problem(good, meta))
        for change in ({'actor': 'browse'}, {'user': 'a'}, {'screen': 'home'}):
            self.assertIsNotNone(report.actor_problem({**good, **change}, meta))

    def test_isolation_checks_require_successful_phases_and_observations(self):
        meta = {'user_isolation': 1, 'phase_schema': 1}
        phases = {k: {'status': 'completed'} for k in ('isolation-setup', 'isolation-loaded-before', 'isolation-loaded-after')}
        (self.root/'phases.json').write_text(json.dumps(phases))
        self.assertIsNotNone(report.isolation_problem(self.root, meta, ['loaded']))
        for edge in ('before', 'after'):
            (self.root/f'isolation-loaded-{edge}.json').write_text(json.dumps({'ok': True, 'changed_items': [], 'home_changed': False}))
        self.assertIsNone(report.isolation_problem(self.root, meta, ['loaded']))
        phases['isolation-loaded-after']['status'] = 'failed'
        (self.root/'phases.json').write_text(json.dumps(phases))
        self.assertIn('isolation-loaded-after', report.isolation_problem(self.root, meta, ['loaded']))


class InstrumentTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def test_sampler_preserves_distinct_observations_in_one_millisecond(self):
        for name, value in {'memory.stat': 'anon 100\nfile 0\n',
                            'memory.current': '100', 'memory.swap.current': '0'}.items():
            (self.root/name).write_text(value)
        output = self.root/'mem.csv'
        timestamps = [1789519227.7521, 1789519227.7523]
        handlers = {}
        sleeps = []

        def sleep(_):
            sleeps.append(True)
            if len(sleeps) == 2:
                handlers[mem_sample.signal.SIGTERM](None, None)

        with patch.object(mem_sample.sys, 'argv', ['sampler', 'container', str(output), '100', '0']), \
             patch.object(mem_sample, 'cgroup_dir', return_value=str(self.root)), \
             patch.object(mem_sample, 'container_cpu_usec', side_effect=[0, 100, 200]), \
             patch.object(mem_sample, 'cpu_times', return_value=(0, 100)), \
             patch.object(mem_sample.signal, 'signal', side_effect=lambda sig, fn: handlers.update({sig: fn})), \
             patch.object(mem_sample.time, 'time', side_effect=timestamps), \
             patch.object(mem_sample.time, 'sleep', side_effect=sleep):
            mem_sample.main()
        recorded = [float(row.split(',')[0]) for row in output.read_text().splitlines()[1:]]
        self.assertEqual(recorded, timestamps)
        (self.root/'windows.json').write_text(json.dumps({'loaded': {'start': timestamps[0], 'end': timestamps[1]}}))
        evidence = report.resource_windows(self.root, {'resource_schema': 1})['loaded']
        self.assertNotIn('error', evidence)
        self.assertAlmostEqual(evidence['cpu_seconds'], .0001)

    def test_shape_cli_names_the_server_whose_response_failed(self):
        shape = {'home:image': {'status': {0, 200}}}
        with patch.object(report.sys, 'argv', ['report', '--shape-coverage', str(self.root/'ferrofin')]), \
             patch.object(report, 'load', return_value={}), \
             patch.object(report, 'load_shape', return_value=shape), \
             patch.object(report, 'shape_evidence', return_value=(None, None, {})), \
             patch('builtins.print'):
            with self.assertRaisesRegex(ValueError, 'ferrofin failed home:image'):
                report.main()

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


class GateTests(unittest.TestCase):
    """The A/A noise table and the A/B verdict (gate.py) over synthetic run dirs."""
    write, edit, make_run, shapes, ferrofin_only = (ReportTests.write, ReportTests.edit, ReportTests.make_run,
                                                    ReportTests.shapes, ReportTests.ferrofin_only)
    SCREEN = {'relative_pct': 15, 'absolute': 2}
    ENDPOINT = {'relative_pct': 15, 'absolute': 0.5}

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def runs(self, prefix, p50s):
        return [str(self.ferrofin_only(f'{prefix}{i}', p50=p)) for i, p in enumerate(p50s)]

    def test_untouched_path_moving_ten_percent_is_within_noise(self):
        # search:persons in the October b5 runs: main vs branch, median-of-3 +10.2%, slower in
        # 2 of 3 pairs, on a path the branch did not touch.
        v, diff, pct, slower = gate.verdict([1.8335, 1.8888, 1.8470], [1.7971, 2.0506, 2.0359], self.ENDPOINT)
        self.assertEqual(v, 'within noise')
        self.assertAlmostEqual(pct, 10.2, places=1)
        self.assertEqual(slower, 2)

    def test_verdict_needs_both_thresholds_and_two_of_three_pairs(self):
        for name, a, b, expected in [
                ('regressed', [10, 10, 10], [13, 12.5, 13], 'regressed'),
                ('improved', [13, 12.5, 13], [10, 10, 10], 'improved'),
                ('relative but not absolute', [1, 1, 1], [1.9, 1.9, 1.9], 'within noise'),
                ('absolute but not relative', [100, 100, 100], [110, 110, 110], 'within noise'),
                ('median moved, one pair slower', [10, 10, 30], [14, 9, 14], 'within noise'),
                ('no threshold row', [10, 10, 10], [20, 20, 20], None)]:
            with self.subTest(name):
                band = None if expected is None else self.SCREEN
                self.assertEqual(gate.verdict(a, b, band)[0], expected or 'within noise')

    def test_stress_and_sub_millisecond_endpoints_never_gate(self):
        self.assertFalse(gate.is_gated('stress/screens/home', 10, 20))
        self.assertFalse(gate.is_gated('loaded/endpoints/home:items', 0.4, 0.9))
        self.assertTrue(gate.is_gated('loaded/endpoints/home:items', 0.8, 2))
        self.assertTrue(gate.is_gated('unloaded/screens/home', 0.4, 0.5))
        self.assertTrue(gate.is_gated('memory/steady idle', 1, 1))

    def test_ab_verdict_blocks_on_a_gated_regression_only(self):
        a, b = report.build(self.runs('a', [10, 10, 10])), report.build(self.runs('b', [20, 19, 21]))
        rows = gate.verdicts(a, b, report.load_gate())
        self.assertEqual(rows['loaded/screens/home']['verdict'], 'regressed')
        self.assertEqual(rows['memory/steady idle']['verdict'], 'within noise')
        self.assertIn('loaded/screens/home', gate.blocking(rows))
        same = gate.verdicts(a, report.build(self.runs('c', [10, 10, 10])), report.load_gate())
        self.assertEqual(gate.blocking(same), [])
        self.assertEqual({r['verdict'] for r in same.values()}, {'within noise'})

    def test_stress_regression_is_reported_not_blocking(self):
        def stress(prefix, p50s):
            out = self.runs(prefix, p50s)
            for run in out:
                d = Path(run) / 'ferrofin'
                (d / 'k6-loaded.json').rename(d / 'k6-stress.json')
                self.write(d / 'windows.json', {'stress': {'start': 1, 'end': 2}, 'steady': {'start': 3, 'end': 4}})
            return report.build(out)
        rows = gate.verdicts(stress('a', [10, 10, 10]), stress('b', [20, 20, 20]), report.load_gate())
        self.assertEqual(rows['stress/screens/home']['verdict'], 'regressed')
        self.assertFalse(rows['stress/screens/home']['gated'])
        self.assertNotIn('stress/screens/home', gate.blocking(rows))

    def test_flagged_cell_cannot_be_judged_and_blocks(self):
        runs = self.runs('b', [20, 20, 20])
        self.edit(Path(runs[1]) / 'ferrofin/k6-loaded.json', lambda x: x['screens']['home'].update(ok=0.5))
        rows = gate.verdicts(report.build(self.runs('a', [10, 10, 10])), report.build(runs), report.load_gate())
        self.assertEqual(rows['loaded/screens/home']['verdict'], 'not comparable')
        self.assertIn('measured requests failed', rows['loaded/screens/home']['reason'])
        self.assertIn('loaded/screens/home', gate.blocking(rows))

    def test_noise_table_spans_the_runs_and_proposes_bands_beyond_the_row(self):
        runs = self.runs('aa', [10, 10, 10, 10, 10, 14])
        noise = gate.noise_table(report.build(runs), report.load_gate())
        home = noise['loaded/screens/home']
        self.assertEqual((home['p50']['min'], home['p50']['max'], home['p50']['median']), (10, 14, 10))
        self.assertAlmostEqual(home['p50']['range_pct'], 40)
        self.assertGreater(home['p50']['cv_pct'], 0)
        self.assertIn('p95', home)
        self.assertTrue(home['exceeds_band'])
        self.assertIn('value', noise['memory/peak under load'])
        self.assertEqual(gate.proposed_cells(noise)['loaded/screens/home'], {'relative_pct': 40, 'absolute': 4.0})
        self.assertNotIn('memory/steady idle', gate.proposed_cells(noise))  # every run agreed
        kinds = gate.kind_summary(noise, report.load_gate())
        self.assertEqual((kinds['screens']['cells'], kinds['screens']['exceeding']), (6, 6))
        self.assertAlmostEqual(kinds['screens']['max_range_pct'], 40)
        self.assertEqual(kinds['memory']['exceeding'], 0)
        self.assertEqual(kinds['screens']['row'], {'relative_pct': 15, 'absolute': 2})

    def test_schedule_interleaves_and_dry_run_starts_nothing(self):
        self.assertEqual([(n, r) for n, r, _ in gate.schedule('ab', {'a': 'old', 'b': 'new'}, 3)],
                         [('01-a', 'a'), ('02-b', 'b'), ('03-a', 'a'), ('04-b', 'b'), ('05-a', 'a'), ('06-b', 'b')])
        self.assertEqual(len(gate.schedule('aa', {'a': 'x'}, 6)), 6)
        out = self.root / 'session'
        result = subprocess.run([sys.executable, str(Path(__file__).parent / 'gate.py'), 'ab', 'old', 'new',
                                 '--dry-run', '--out', str(out)], capture_output=True, text=True,
                                env=dict(os.environ, PATH=''))  # no docker reachable
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.count('--only shape,unloaded,loaded'), 6)
        self.assertIn('06-b  b  new', result.stdout)
        self.assertFalse(out.exists())

    def test_driver_retags_each_run_stops_on_a_wrong_image_and_restores_the_tag(self):
        plan = {'schedule': gate.schedule('ab', {'a': 'old', 'b': 'new'}, 3),
                'images': {'a': {'id': 'sha256:old'}, 'b': {'id': 'sha256:new'}}}
        calls = []

        def docker(*args):
            calls.append(args)
            return 'sha256:orig' if args[:2] == ('image', 'inspect') else ''

        def run(cmd):
            out = Path(cmd[cmd.index('--out') + 1]) / 'ferrofin'
            out.mkdir(parents=True)
            tagged = calls[-1][1]  # the image the driver just tagged ferrofin:bench to
            # the second run finds ferrofin:bench retagged under it by someone else
            (out / 'image.txt').write_text('ferrofin:bench ' + ('sha256:old' if out.parent.name == '02-b' else tagged))
            return 0

        with patch.object(gate, 'docker', docker), patch.object(gate, 'run_sh', run):
            with self.assertRaisesRegex(SystemExit, '02-b measured'):
                gate.run_session(str(self.root), plan, None)
        self.assertEqual([c for c in calls if c[0] == 'tag'],
                         [('tag', 'sha256:old', 'ferrofin:bench'), ('tag', 'sha256:new', 'ferrofin:bench'),
                          ('tag', 'sha256:orig', 'ferrofin:bench')])
        self.assertFalse((self.root / '03-a').exists())

    def test_a_run_that_cannot_start_stops_the_session_with_its_own_exit(self):
        plan = {'schedule': gate.schedule('aa', {'a': 'x'}, 2), 'images': {'a': {'id': 'sha256:x'}}}
        with patch.object(gate, 'docker', lambda *a: ''), patch.object(gate, 'run_sh', lambda cmd: 1):
            with self.assertRaisesRegex(SystemExit, r'01-a failed \(run.sh exit 1\)'):
                gate.run_session(str(self.root), plan, None)

    def test_run_counts_are_checked_before_anything_starts(self):
        script = str(Path(__file__).parent / 'gate.py')
        for args in (['aa', 'x', '--runs', '3'], ['aa', 'x', '--runs', '0'], ['ab', 'x', 'y', '--pairs', '0'],
                     ['ab', 'x', 'y', '--pairs', 'two'], ['render']):
            with self.subTest(args):
                result = subprocess.run([sys.executable, script, *args, '--dry-run'] if args != ['render'] else
                                        [sys.executable, script, *args], capture_output=True, text=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn('Traceback', result.stderr)

    def session(self, mode, p50s):
        out = self.root / f'gate-{mode}'
        plan = {'mode': mode, 'name': out.name, 'date': '2026-10-19T00:00:00+00:00',
                'schedule': gate.schedule(mode, {'a': 'old', 'b': 'new'}, len(p50s) // (2 if mode == 'ab' else 1)),
                'images': {'a': {'ref': 'old', 'id': 'sha256:old'}, 'b': {'ref': 'new', 'id': 'sha256:new'}}}
        out.mkdir()
        for (name, _, _), p in zip(plan['schedule'], p50s):
            shutil.move(str(self.ferrofin_only(name, p50=p)), out / name)
        self.write(out / 'gate.json', plan)
        return out

    def test_render_writes_the_session_report_and_its_machine_readable_file(self):
        out = self.session('ab', [10, 20, 10, 20, 10, 20])
        md, block = gate.render(str(out))
        self.assertIn('loaded/screens/home', block)
        self.assertIn('**BLOCKED**', md)
        page = (out / 'report.html').read_text()
        self.assertIn("class='delta bad'>regressed", page)
        self.assertEqual(json.loads((out / 'verdict.json').read_text())['blocking'], block)
        aa = self.session('aa', [10, 10, 10, 10, 10, 10])
        md, block = gate.render(str(aa))
        self.assertEqual(block, [])
        noise = json.loads((aa / 'noise.json').read_text())
        self.assertEqual(noise['proposed_cells'], {})
        self.assertEqual({r['verdict'] for r in noise['false_alarms'].values()}, {'within noise'})
        self.assertIn('Noise per cell', (aa / 'report.html').read_text())

    def scan_session(self, name, candidate_walls):
        """A scan session: the previous release scans in 100 s every run, the candidate in
        `candidate_walls` (one per pair); memory and migration are equal on both sides."""
        out = self.root / name
        plan = {'mode': 'scan', 'name': name, 'date': '2026-10-20T00:00:00+00:00',
                'schedule': gate.schedule('scan', {'a': 'old', 'b': 'new'}, len(candidate_walls)),
                'images': {'a': {'ref': 'old', 'id': 'sha256:old'}, 'b': {'ref': 'new', 'id': 'sha256:new'}}}
        walls = iter(w for b in candidate_walls for w in (100, b))
        for run, _, _ in plan['schedule']:
            self.write(out / run / 'scan.json', {'scan': {'wall_s': next(walls), 'peak_anon_mib': 500},
                                                 'migration': {'wall_s': 2.5, 'peak_anon_mib': 300}})
        self.write(out / 'gate.json', plan)
        return out

    def test_scan_session_blocks_a_slower_scan_and_passes_a_self_ab(self):
        md, block = gate.render(str(self.scan_session('slower', [120, 118, 121])))
        self.assertEqual(block, ['scan/wall time'])
        self.assertIn('**BLOCKED**', md)
        page = (self.root / 'slower' / 'report.html').read_text()
        self.assertIn("class='delta bad'>regressed", page)
        self.assertIn('migration/peak anon', page)
        md, block = gate.render(str(self.scan_session('self', [101, 99, 100])))
        self.assertEqual(block, [])
        rows = json.loads((self.root / 'self' / 'verdict.json').read_text())['verdicts']
        self.assertEqual({r['verdict'] for r in rows.values()}, {'within noise'})
        self.assertEqual(set(rows), {c[0] for c in gate.SCAN_CELLS})

    def test_scan_session_without_a_measurement_is_a_readable_error(self):
        out = self.scan_session('partial', [100, 100, 100])
        (out / '06-b' / 'scan.json').unlink()
        with self.assertRaisesRegex(ValueError, '06-b has no scan.json'):
            gate.render(str(out))

    def test_scan_driver_runs_scan_py_on_the_image_id_and_stops_at_a_failure(self):
        plan = {'mode': 'scan', 'schedule': gate.schedule('scan', {'a': 'old', 'b': 'new'}, 3),
                'images': {'a': {'ref': 'old', 'id': 'sha256:old'}, 'b': {'ref': 'new', 'id': 'sha256:new'}}}
        cmds = []

        def run(cmd, wrong=None, fail=None):
            cmds.append(cmd)
            out = Path(cmd[-1])
            if out.name == fail:
                return 1
            self.write(out / 'scan.json', {'image_id': 'sha256:old' if out.name == wrong else cmd[2]})
            return 0

        with patch.object(gate, 'docker', Mock(side_effect=AssertionError('nothing is retagged'))):
            with patch.object(gate, 'run_sh', lambda cmd: run(cmd, fail='02-b')):
                with self.assertRaisesRegex(SystemExit, r'02-b failed \(scan.py exit 1\)'):
                    gate.run_session(str(self.root / 's1'), plan, '/data')
            self.assertEqual([c[1:] for c in cmds],
                             [[str(Path(gate.BENCH) / 'scan.py'), 'sha256:old', '/data', str(self.root / 's1' / '01-a')],
                              [str(Path(gate.BENCH) / 'scan.py'), 'sha256:new', '/data', str(self.root / 's1' / '02-b')]])
            with patch.object(gate, 'run_sh', lambda cmd: run(cmd, wrong='04-b')):
                with self.assertRaisesRegex(SystemExit, '04-b measured sha256:old, not new'):
                    gate.run_session(str(self.root / 's2'), plan, '/data')

    def test_scan_dry_run_lists_scan_runs_and_starts_nothing(self):
        out = self.root / 'session'
        result = subprocess.run([sys.executable, str(Path(__file__).parent / 'gate.py'), 'scan', 'old', 'new',
                                 '--dry-run', '--out', str(out)], capture_output=True, text=True,
                                env=dict(os.environ, PATH=''))  # no docker reachable
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.count('scan.py'), 6)
        self.assertNotIn('run.sh', result.stdout)
        self.assertFalse(out.exists())

    def test_scan_peak_is_the_largest_anon_sample_inside_the_span(self):
        path = self.root / 'mem.csv'
        mib = 2 ** 20
        path.write_text('t,anon,file\n' + ''.join(f'{t},{a * mib},0\n' for t, a in
                                                  [(9.9, 900), (10.0, 100), (10.5, 400), (11.0, 300), (11.1, 800)]))
        self.assertEqual(scan.peak_anon_mib(path, 10.0, 11.0), 400)
        with self.assertRaisesRegex(RuntimeError, 'no memory sample'):
            scan.peak_anon_mib(path, 12, 13)

    def test_scan_cleanup_never_masks_the_measurement_error_and_always_removes_the_container(self):
        def server():
            s = scan.Server.__new__(scan.Server)  # no docker create
            s.d, s.sampler = str(self.root), None
            return s

        calls = []

        def docker(*args):
            calls.append(args)
            raise RuntimeError('docker rm: no such container')

        with patch.object(scan, 'docker', docker), patch.object(scan.subprocess, 'run'), \
                patch('sys.stderr', new_callable=io.StringIO):
            with self.assertRaisesRegex(ValueError, 'the measurement failed'):
                with server():
                    raise ValueError('the measurement failed')
            with self.assertRaisesRegex(RuntimeError, 'docker rm'):
                with server():
                    pass
        self.assertEqual(calls, [('rm', '-f', 'bench-scan')] * 2)

    def test_migration_copy_turns_the_realtime_monitor_off(self):
        lib = self.root / 'root' / 'default' / 'Movies'
        lib.mkdir(parents=True)
        (lib / 'options.xml').write_text('<LibraryOptions><EnableRealtimeMonitor>true</EnableRealtimeMonitor></LibraryOptions>')
        scan.disable_realtime_monitor(self.root)
        self.assertIn('<EnableRealtimeMonitor>false</EnableRealtimeMonitor>', (lib / 'options.xml').read_text())
        with self.assertRaisesRegex(RuntimeError, 'no library options'):
            scan.disable_realtime_monitor(self.root / 'empty')

    def test_scan_counts_fail_on_fewer_items_and_record_more(self):
        summary = {'movies': 3001, 'series': 250, 'episodes': 7490, 'albums': 800, 'tracks': 8000}
        by_type = {'Movie': 3041, 'Series': 250, 'Episode': 7490, 'MusicAlbum': 800, 'Audio': 8000}
        api = Mock(get=lambda path, includeItemTypes, **q: {'TotalRecordCount': by_type[includeItemTypes]})
        self.assertEqual(scan.counts(api, 'u', summary)['movies'], 3041)  # v1.3.2 lists alternate versions
        by_type['Episode'] = 7489
        with self.assertRaisesRegex(RuntimeError, r"fewer items.*'episodes': '7489 \(fixture 7490\)'"):
            scan.counts(api, 'u', summary)


if __name__ == '__main__':
    unittest.main()
