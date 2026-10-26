"""The local report viewer (`report.py --serve`): run discovery, the tab index, the
side-by-side compare and the served reports, against synthetic run directories."""
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import threading
import unittest
import urllib.parse
from unittest.mock import patch
import urllib.error
import urllib.request

import report
import test_report


class ViewerTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        # the benchmark fixtures of the reporter tests: two complete runs, `a` and `b`
        fixtures = test_report.ReportTests('make_run')
        fixtures.setUp()
        self.addCleanup(fixtures.tmp.cleanup)
        self.bench = fixtures.root
        self.adoption = self.root / 'adoption'
        self.parity = self.root / 'parity'
        self.verify = self.root / 'verify'
        self.roots = {'benchmark': str(self.bench), 'adoption': str(self.adoption), 'parity': str(self.parity),
                      'verify': str(self.verify)}

    def report_html(self, root, name, text):
        run = root / name
        run.mkdir(parents=True, exist_ok=True)
        (run / 'report.html').write_text(text)

    def get(self, target, roots=None):
        status, kind, body = report.handle(target, roots or self.roots)
        return status, kind, body.decode('utf-8')

    def test_discovery_lists_benchmark_runs_by_run_json(self):
        (self.bench / 'stray').mkdir()
        self.assertEqual({n for n, _ in report.discover('benchmark', str(self.bench))}, {'a', 'b'})

    def test_discovery_lists_only_directories_with_a_report(self):
        self.report_html(self.adoption, 'one', '<p>one</p>')
        self.report_html(self.adoption, 'two', '<p>two</p>')
        (self.adoption / 'no-report').mkdir()
        self.assertEqual({n for n, _ in report.discover('adoption', str(self.adoption))}, {'one', 'two'})

    def test_discovery_accepts_the_run_directory_itself(self):
        self.report_html(self.root, 'adoption', '<p>work dir</p>')
        self.assertEqual([n for n, _ in report.discover('adoption', str(self.adoption))], ['adoption'])

    def test_one_directory_of_every_gate_s_runs_lists_each_in_its_own_tab(self):
        runs = self.root / 'runs'   # the output root's runs/ (gates/lib.sh): every run has a report.html
        for name in ('bench-v1', 'parity-20261024-1200-abc', 'verify-20261024-1201', 'old-unprefixed'):
            self.report_html(runs, name, f'<p>{name}</p>')
            (runs / name / 'run.json').write_text('{"date": "2026-10-24T12:00:00"}')
        listed = {h: {n for n, _ in report.discover(h, str(runs))} for h in ('benchmark', 'parity', 'adoption', 'verify')}
        self.assertEqual(listed, {'benchmark': {'bench-v1', 'old-unprefixed'},
                                  'parity': {'parity-20261024-1200-abc', 'old-unprefixed'},
                                  'adoption': {'old-unprefixed'},
                                  'verify': {'verify-20261024-1201', 'old-unprefixed'}})

    def test_a_gates_run_session_lists_its_benchmark_run_or_its_ab_gate_runs(self):
        runs = self.root / 'runs'
        for session, files in (('s1', ['bench/run.json']), ('s2', ['bench/gate.json', 'bench/report.html', 'bench/01-a/run.json'])):
            (runs / session).mkdir(parents=True)
            (runs / session / 'session.json').write_text('{}')
            for f in files:
                (runs / session / f).parent.mkdir(parents=True, exist_ok=True)
                (runs / session / f).write_text('{"date": "2026-10-24T12:00:00", "mode": "ab"}')
        self.assertEqual({n for n, _ in report.discover('benchmark', str(runs))}, {'s1/bench', 's2/bench', 's2/bench/01-a'})
        self.report_html(runs / 'parity-x-20261024-1200', 'parity', '<p>a session of a branch named parity-x</p>')
        (runs / 'parity-x-20261024-1200' / 'session.json').write_text('{}')
        self.assertEqual({n for n, _ in report.discover('parity', str(runs))}, {'parity-x-20261024-1200/parity'})
        status, _, body = self.get('/report/parity/parity-x-20261024-1200/parity', {**self.roots, 'parity': str(runs)})
        self.assertEqual((status, body), (200, '<p>a session of a branch named parity-x</p>'))

    def verify_result(self, run, verdict='passed'):
        """What gates/verify/run.sh leaves: a result.json (and server.log), from verify's own code."""
        run.mkdir(parents=True, exist_ok=True)
        (run / 'verify.json').write_text(json.dumps({'exit': 1, 'libraries': {
            'movies': {'rows': [{'row': 2, 'scenario': 'rescan, nothing changed', 'result': 'FAIL' if verdict == 'failed' else 'PASS',
                                 'detail': 'updated 1 (want = 0)',
                                 'checks': [{'what': 'updated', 'observed': 1, 'op': 'eq', 'expected': 0, 'ok': False}]}]},
            'tvshows': None}}))
        (run / 'server.log').write_text('log\n')
        spec = importlib.util.spec_from_file_location('verify_report_for_tests', Path(report.__file__).parent.parent / 'verify' / 'verify_report.py')
        verify_report = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(verify_report)
        verify_report.result.write(str(run), verify_report.build(str(run), 'ferrofin:v9'))

    def test_verify_tab_lists_standalone_runs_and_session_gates_by_their_result(self):
        runs = self.root / 'runs'
        self.verify_result(runs / 'verify-20261025-1200', 'failed')
        self.verify_result(runs / 'release-v1.4.0-20261025-1300' / 'verify')
        (runs / 'release-v1.4.0-20261025-1300' / 'session.json').write_text('{}')
        (runs / 'release-v1.4.0-20261025-1400').mkdir()   # a session that did not run verify
        (runs / 'release-v1.4.0-20261025-1400' / 'session.json').write_text('{}')
        (runs / 'verify-20261025-1500').mkdir()   # a run that left nothing
        self.assertEqual({n for n, _ in report.discover('verify', str(runs))},
                         {'verify-20261025-1200', 'release-v1.4.0-20261025-1300/verify'})
        roots = {**self.roots, 'verify': str(runs)}
        status, _, page = self.get('/report/verify/verify-20261025-1200', roots)
        self.assertEqual(status, 200)
        for text in ('<h1>Verify: failed</h1>', 'rescan, nothing changed', "<td class='num bad'>1</td>", '0 PASS · 1 FAIL · 0 SKIP'):
            self.assertIn(text, page)
        self.assertNotIn('server.log', page)   # the viewer serves the page alone: a relative log link would not resolve
        self.assertEqual(self.get('/report/verify/release-v1.4.0-20261025-1300/verify', roots)[0], 200)
        _, _, tab = self.get('/verify', roots)
        self.assertIn("href='/report/verify/verify-20261025-1200'", tab)

    def test_a_verify_report_on_disk_is_served_as_written(self):
        self.verify_result(self.verify / 'verify-1')
        (self.verify / 'verify-1' / 'report.html').write_text('<p>static</p>')
        self.assertEqual(self.get('/report/verify/verify-1')[2], '<p>static</p>')

    def test_an_invalid_verify_result_is_a_readable_error(self):
        (self.verify / 'verify-1').mkdir(parents=True)
        (self.verify / 'verify-1' / 'result.json').write_text('{"schema": 1}')
        status, _, page = self.get('/report/verify/verify-1')
        self.assertEqual(status, 400)
        self.assertIn('result.json is not a valid result', page)

    def test_discovery_without_a_directory_is_empty(self):
        self.assertEqual(report.discover('parity', None), [])
        self.assertEqual(report.discover('parity', str(self.root / 'missing')), [])

    def test_index_has_four_tabs_and_lists_benchmark_runs(self):
        status, kind, page = self.get('/')
        self.assertEqual((status, kind), (200, 'text/html; charset=utf-8'))
        for tab in ('/benchmark', '/adoption', '/parity', '/verify'):
            self.assertIn(f"href='{tab}'", page)
        self.assertIn("href='/report/benchmark/a'", page)
        self.assertIn("href='/report/benchmark/b'", page)

    def test_empty_tab_says_where_its_runs_come_from(self):
        _, _, adoption = self.get('/adoption')
        self.assertIn('No adoption runs', adoption)
        self.assertIn('--adoption-dir', adoption)
        self.assertIn('FERROFIN_ADOPTION_RUNS', adoption)
        _, _, parity = self.get('/parity', {**self.roots, 'parity': None})
        self.assertIn('--parity-dir', parity)

    def test_tab_lists_the_runs_of_its_harness_only(self):
        self.report_html(self.adoption, 'run-1', '<p>x</p>')
        self.report_html(self.parity, 'parity-1', '<p>y</p>')
        _, _, adoption = self.get('/adoption')
        self.assertIn("href='/report/adoption/run-1'", adoption)
        self.assertNotIn('parity-1', adoption)
        self.assertNotIn("/report/benchmark/", adoption)

    def test_report_is_served_as_written(self):
        self.report_html(self.adoption, 'run-1', '<!doctype html><p>adoption ok</p>')
        status, kind, page = self.get('/report/adoption/run-1')
        self.assertEqual((status, kind), (200, 'text/html; charset=utf-8'))
        self.assertEqual(page, '<!doctype html><p>adoption ok</p>')

    def test_unknown_and_escaping_run_names_are_404(self):
        self.report_html(self.adoption, 'run-1', '<p>x</p>')
        (self.root / 'secret').mkdir()
        (self.root / 'secret' / 'report.html').write_text('secret')
        for target in ('/report/adoption/nope', '/report/adoption/..%2Fsecret', '/report/adoption/../secret',
                       '/report/other/run-1', '/report/adoption', '/elsewhere'):
            with self.subTest(target):
                self.assertEqual(self.get(target)[0], 404)

    def test_compare_puts_two_runs_of_one_harness_side_by_side(self):
        self.report_html(self.adoption, 'old', '<p>old</p>')
        self.report_html(self.adoption, 'new', '<p>new</p>')
        _, _, page = self.get('/adoption?a=old&b=new')
        self.assertLess(page.index("src='/report/adoption/old'"), page.index("src='/report/adoption/new'"))
        self.assertEqual(page.count('<iframe'), 2)

    def test_compare_ignores_a_run_the_harness_does_not_have(self):
        self.report_html(self.adoption, 'old', '<p>old</p>')
        _, _, page = self.get('/adoption?a=old&b=../benchmark/a')
        self.assertEqual(page.count('<iframe'), 1)
        self.assertNotIn('benchmark/a', page)

    def test_benchmark_run_without_a_static_report_is_rendered_on_demand(self):
        status, _, page = self.get('/report/benchmark/a')
        self.assertEqual(status, 200)
        self.assertIn('Ferrofin vs Jellyfin', page)

    def test_benchmark_static_report_wins_over_rendering(self):
        self.report_html(self.bench, 'a', '<p>static a</p>')
        self.assertEqual(self.get('/report/benchmark/a')[2], '<p>static a</p>')

    def test_median_picker_still_renders_runs_and_baselines(self):
        status, _, page = self.get('/?run=a&baseline=b')
        self.assertEqual(status, 200)
        self.assertIn('Ferrofin vs Jellyfin', page)
        self.assertIn("href='/adoption'", page)

    def test_unrenderable_benchmark_run_is_a_readable_error(self):
        for ex, status in ((ValueError('incomplete run'), 400), (KeyError('x'), 500)):
            with self.subTest(ex=ex), patch.object(report, 'build', side_effect=ex):
                got, _, page = self.get('/report/benchmark/a')
                self.assertEqual(got, status)
                self.assertIn('Could not render a', page)
                self.assertIn("href='/adoption'", page)

    def test_write_html_leaves_a_self_contained_report_beside_the_run(self):
        run = self.bench / 'a'
        path = report.write_html(str(run))
        self.assertEqual(Path(path), run / 'report.html')
        page = (run / 'report.html').read_text()
        self.assertIn('<style>', page)
        self.assertNotRegex(page, r"(src|href)=['\"]https?://")

    def test_ab_verdicts_appear_in_the_run_report(self):
        run = self.bench / 'a'
        (run / 'ab-verdicts.json').write_text(json.dumps({'cells': [
            {'cell': 'loaded · screens · home', 'verdict': 'regressed', 'baseline': 10, 'candidate': 14, 'unit': 'ms', 'delta_pct': 40},
            {'cell': 'loaded · screens · movies', 'verdict': 'within noise', 'baseline': 10, 'candidate': 10.5, 'unit': 'ms', 'delta_pct': 5},
            {'cell': 'memory · peak', 'verdict': 'improved', 'baseline': 400, 'candidate': 300, 'unit': 'MiB', 'delta_pct': -25}]}))
        page = report.render_html(report.build([str(run)]), verdicts=report.load_verdicts([str(run)]))
        for text in ('A/B gate', 'regressed', 'within noise', 'improved', 'loaded · screens · home', '+40.0%', '-25.0%'):
            self.assertIn(text, page)

    def test_run_without_verdicts_has_no_gate_section(self):
        run = self.bench / 'a'
        self.assertIsNone(report.load_verdicts([str(run)]))
        self.assertNotIn('A/B gate', report.render_html(report.build([str(run)])))

    def test_malformed_verdicts_are_a_readable_error(self):
        run = self.bench / 'a'
        (run / 'ab-verdicts.json').write_text('{"cells": [{"cell": "x"}]}')
        with self.assertRaisesRegex(ValueError, 'ab-verdicts.json'):
            report.load_verdicts([str(run)])

    def test_run_names_with_markup_are_escaped_and_round_trip(self):
        name = "a 'b' & <c>"
        self.report_html(self.adoption, name, '<p>odd</p>')
        _, _, page = self.get('/adoption?a=' + urllib.parse.quote(name))
        self.assertNotIn('<c>', page)
        self.assertIn("/report/adoption/a%20%27b%27%20%26%20%3Cc%3E", page)
        self.assertEqual(self.get('/report/adoption/' + urllib.parse.quote(name))[2], '<p>odd</p>')

    def test_a_work_dir_report_is_served_as_the_root_run(self):
        self.report_html(self.root, 'adoption', '<p>work dir</p>')
        self.assertEqual(self.get('/report/adoption/adoption')[2], '<p>work dir</p>')

    def test_root_and_same_named_subdirectory_are_one_run(self):
        self.report_html(self.root, 'adoption', '<p>root</p>')
        self.report_html(self.adoption, 'adoption', '<p>sub</p>')
        self.assertEqual([n for n, _ in report.discover('adoption', str(self.adoption))], ['adoption'])
        self.assertEqual(self.get('/report/adoption/adoption')[2], '<p>sub</p>')

    def test_unknown_run_in_the_picker_query_keeps_the_picker(self):
        status, _, page = self.get('/?run=nonexistent')
        self.assertEqual(status, 200)
        self.assertIn("name='run' value='a'", page)

    def test_render_error_keeps_the_tabs_and_the_picker(self):
        with patch.object(report, 'build', side_effect=ValueError('incomplete run')):
            status, _, page = self.get('/?run=a')
        self.assertEqual(status, 400)
        self.assertIn("name='run' value='a'", page)
        self.assertIn("href='/parity'", page)

    def test_compare_page_carries_the_tab_styles(self):
        self.assertIn('nav.tabs', self.get('/?run=a')[2])

    def test_failed_write_html_leaves_no_report_behind(self):
        run = self.bench / 'a'
        (run / 'ab-verdicts.json').write_text('{"cells": [{"cell": "x"}]}')
        with self.assertRaises(ValueError):
            report.write_html(str(run))
        self.assertFalse((run / 'report.html').exists())
        self.assertFalse((run / 'report.html.tmp').exists())

    def test_verdicts_reach_every_page_the_viewer_serves(self):
        (self.bench / 'a' / 'ab-verdicts.json').write_text(json.dumps({'cells': [
            {'cell': 'memory · peak', 'verdict': 'improved', 'delta_pct': -25}]}))
        for target in ('/report/benchmark/a', '/?run=a'):
            with self.subTest(target):
                self.assertIn('A/B gate', self.get(target)[2])

    def test_a_gate_with_no_cells_is_still_shown(self):
        (self.bench / 'a' / 'ab-verdicts.json').write_text('{"cells": []}')
        self.assertIn('A/B gate — no cells', self.get('/report/benchmark/a')[2])

    def test_verdict_unit_must_be_text(self):
        (self.bench / 'a' / 'ab-verdicts.json').write_text('{"cells": [{"cell": "x", "verdict": "improved", "unit": 5}]}')
        with self.assertRaisesRegex(ValueError, 'ab-verdicts.json'):
            report.load_verdicts([str(self.bench / 'a')])

    def test_serve_takes_directories_from_flags_then_environment(self):
        env = {'FERROFIN_ADOPTION_RUNS': str(self.adoption), 'FERROFIN_PARITY_RUNS': str(self.root / 'missing'),
               'FERROFIN_VERIFY_RUNS': str(self.verify)}
        cases = [(['--serve', '1234', str(self.bench), '--parity-dir', str(self.parity), '--verify-dir', str(self.adoption)], 1234,
                  {'benchmark': str(self.bench), 'adoption': str(self.adoption), 'parity': str(self.parity),
                   'verify': str(self.adoption)}),
                 (['--serve', str(self.bench)], 8097,
                  {'benchmark': str(self.bench), 'adoption': str(self.adoption), 'parity': None, 'verify': str(self.verify)})]
        for argv, port, roots in cases:
            with self.subTest(argv), patch.dict('os.environ', env), patch.object(report.sys, 'argv', ['report.py', *argv]), \
                    patch.object(report, 'serve') as serve:
                self.parity.mkdir(exist_ok=True)
                self.adoption.mkdir(exist_ok=True)
                self.verify.mkdir(exist_ok=True)
                report.main()
                serve.assert_called_once_with(port, roots)

    def test_serve_takes_every_tab_from_the_output_root_when_nothing_names_one(self):
        gates = self.root / 'gates'
        runs, work = gates / 'runs', gates / 'work'
        cases = [  # (argv, environment beyond the root, the tabs served)
            (['--serve'], {}, {'benchmark': str(runs), 'adoption': str(work), 'parity': str(runs), 'verify': str(runs)}),
            # a positional RUNS_DIR, a flag and a variable each still win over the root
            (['--serve', str(self.bench), '--parity-dir', str(self.parity)], {'FERROFIN_ADOPTION_RUNS': str(self.adoption)},
             {'benchmark': str(self.bench), 'adoption': str(self.adoption), 'parity': str(self.parity), 'verify': str(runs)}),
            # a variable naming a missing directory is an empty tab, not the root's
            (['--serve'], {'FERROFIN_PARITY_RUNS': str(self.root / 'missing')},
             {'benchmark': str(runs), 'adoption': str(work), 'parity': None, 'verify': str(runs)}),
        ]
        self.parity.mkdir()
        self.adoption.mkdir()
        work.mkdir(parents=True)
        for create_runs in (True, False):
            if create_runs:
                runs.mkdir()
            else:  # an adoption-only root has no runs/ yet: empty benchmark and parity tabs, not an exit
                runs.rmdir()
                cases = [(['--serve'], {}, {'benchmark': str(runs), 'adoption': str(work), 'parity': None, 'verify': None})]
            for argv, env, tabs in cases:
                with self.subTest(argv=argv, env=env, runs=create_runs), \
                        patch.dict('os.environ', {'FERROFIN_GATES_DIR': str(gates), **env}, clear=True), \
                        patch.object(report.sys, 'argv', ['report.py', *argv]), patch.object(report, 'serve') as serve, \
                        patch.object(report.sys, 'stderr', new_callable=io.StringIO) as stderr:
                    report.main()
                    serve.assert_called_once_with(8097, tabs)
                    if not create_runs:
                        self.assertIn(f"{runs} (from FERROFIN_GATES_DIR) is not a directory: the benchmark tab stays empty",
                                      stderr.getvalue())
                        self.assertIn(f"{runs} (from FERROFIN_GATES_DIR) is not a directory: the parity tab stays empty",
                                      stderr.getvalue())
        # a RUNS_DIR given on the command line that is missing still refuses, however it is spelled
        for missing in (str(runs), str(runs) + '/', str(gates / 'other')):
            with self.subTest(missing=missing), patch.dict('os.environ', {'FERROFIN_GATES_DIR': str(gates)}, clear=True), \
                    patch.object(report.sys, 'argv', ['report.py', '--serve', missing]), patch.object(report, 'serve'):
                with self.assertRaisesRegex(SystemExit, 'is not a directory'):
                    report.main()

    def test_serve_refuses_a_missing_flag_directory_and_leftover_arguments(self):
        for argv in (['--serve', '--adoption-dir'], ['--serve', '--adoption-dir', str(self.root / 'missing')],
                     ['--serve', str(self.bench), 'extra'], ['--serve', str(self.root / 'missing')]):
            with self.subTest(argv), patch.dict('os.environ', {}, clear=True), \
                    patch.object(report.sys, 'argv', ['report.py', *argv]), patch.object(report, 'serve'):
                with self.assertRaises(SystemExit):
                    report.main()

    def test_server_binds_loopback_only_and_answers_over_http(self):
        self.report_html(self.adoption, 'run-1', '<p>over http</p>')
        srv = report.make_server(0, self.roots)
        self.addCleanup(srv.server_close)
        self.assertEqual(srv.server_address[0], '127.0.0.1')
        thread = threading.Thread(target=srv.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(thread.join, 5)
        self.addCleanup(srv.shutdown)
        base = f'http://127.0.0.1:{srv.server_address[1]}'
        with urllib.request.urlopen(f'{base}/report/adoption/run-1') as response:
            self.assertEqual(response.read(), b'<p>over http</p>')
        with self.assertRaises(urllib.error.HTTPError) as caught:
            urllib.request.urlopen(f'{base}/report/adoption/missing')
        self.addCleanup(caught.exception.close)
        self.assertEqual(caught.exception.code, 404)


if __name__ == '__main__':
    unittest.main()
