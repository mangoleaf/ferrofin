"""The per-endpoint latency pass: the catalogue it reads, the measuring loop, the percentiles,
the A/B verdict per endpoint, the gate session's report and run.sh's arguments (stdlib only;
the servers are local stand-ins, no docker, no k6)."""
import contextlib
import http.server
import json
import os
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path

import endpoints
import gate
import report

BENCH = Path(__file__).parent
ENDPOINTS_PY = str(BENCH / "endpoints.py")
GATE_PY = str(BENCH / "gate.py")
RUN_SH = str(BENCH / "run.sh")
BAND = {"relative_pct": 15, "absolute": 0.5}


def write_set(root, files):
    """A request set: `{"dir/file.jsonl": [request, ...]}` under `root`; returns its directories."""
    dirs = []
    for name, rows in files.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("".join(json.dumps(r) + "\n" for r in rows))
        if path.parent not in dirs:
            dirs.append(path.parent)
    return dirs


def get(rid, path="/Items", **extra):
    return {"id": rid, "method": "GET", "path": path, **extra}


class CatalogueTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def test_only_gets_are_measured_and_only_the_screens_and_edge_files_gate(self):
        dirs = write_set(self.root, {
            "sweep/combos.jsonl": [get("combo")],
            "sweep/contract.jsonl": [get("op"), {"id": "head", "method": "HEAD", "path": "/Items"},
                                     {"id": "w", "method": "POST", "path": "/Playlists",
                                      "readbacks": [{"id": "items", "path": "/Playlists/{write.Id}/Items"}]}],
            "sweep/screens.jsonl": [get("home")],
            "edge/edge.jsonl": [get("edge-paging-first")],
        })
        got = endpoints.catalogue(endpoints.diff.load_requests(dirs))
        self.assertEqual([(e.request.id, e.gates) for e in got],
                         [("combo", False), ("op", False), ("home", True), ("edge-paging-first", True)])

    def test_a_skipped_request_keeps_its_reason(self):
        dirs = write_set(self.root, {"sweep/contract.jsonl": [get("GetVideoStream", "/Videos/{movie}/stream"), get("op")]})
        got = {e.request.id: e.skip for e in endpoints.catalogue(endpoints.diff.load_requests(dirs))}
        self.assertIn("media", got["GetVideoStream"])
        self.assertEqual(got["op"], "")

    def test_every_skip_names_a_get_of_the_real_catalogue(self):
        cat = endpoints.catalogue(endpoints.diff.load_requests(endpoints.default_sets()))
        ids = {e.request.id for e in cat}
        self.assertEqual(sorted(set(endpoints.SKIP) - ids), [])
        self.assertTrue(all(reason for reason in endpoints.SKIP.values()))

    def test_the_real_catalogue_gates_the_screens_shapes_and_the_edge_gets(self):
        cat = endpoints.catalogue(endpoints.diff.load_requests(endpoints.default_sets()))
        gated = [e for e in cat if e.gates]
        self.assertTrue(gated)
        self.assertEqual({e.request.file for e in gated}, set(endpoints.GATED_FILES))
        self.assertFalse([e.request.id for e in gated if e.skip], "a gated endpoint is never skipped")
        self.assertGreater(len(cat), 2000)


class PercentileTests(unittest.TestCase):
    def test_linear_interpolation_between_ranks(self):
        vals = [float(v) for v in range(1, 101)]
        self.assertAlmostEqual(endpoints.percentile(vals, 50), 50.5)
        self.assertAlmostEqual(endpoints.percentile(vals, 95), 95.05)
        self.assertAlmostEqual(endpoints.percentile(vals, 99), 99.01)

    def test_one_sample_is_every_percentile(self):
        self.assertEqual(endpoints.percentile([7.0], 99), 7.0)

    def test_no_samples_have_no_percentile(self):
        self.assertIsNone(endpoints.percentile([], 50))

    def test_summary_counts_answers_other_than_the_expected_status(self):
        s = endpoints.summarise([(200, 1.0), (200, 3.0), (500, 2.0), (None, 10.0)], 200)
        self.assertEqual((s["n"], s["errors"]), (4, 2))
        self.assertEqual(s["statuses"], {"200": 2, "500": 1, "none": 1})
        self.assertEqual(s["p50"], 2.5)


class FakeClock:
    """A clock each request advances by its cost, so the loop's stop rule is exact."""

    def __init__(self):
        self.t = 0.0

    def __call__(self):
        return self.t


class MeasureTests(unittest.TestCase):
    def send_costing(self, clock, ms, status=200):
        calls = []

        def send(path):
            calls.append(path)
            clock.t += ms / 1000
            return status, ms
        return send, calls

    def test_stops_at_the_count_when_requests_are_fast(self):
        clock = FakeClock()
        send, calls = self.send_costing(clock, 2)
        samples = endpoints.measure(send, "/x", count=200, max_s=10, clock=clock)
        self.assertEqual(len(samples), 200)
        self.assertEqual(len(calls), 200)

    def test_stops_at_the_time_limit_when_requests_are_slow(self):
        clock = FakeClock()
        send, _ = self.send_costing(clock, 300)
        samples = endpoints.measure(send, "/x", count=200, max_s=10, clock=clock)
        self.assertEqual(len(samples), 34)  # the request that crosses 10 s still counts

    def test_a_request_slower_than_the_limit_is_one_sample(self):
        clock = FakeClock()
        send, _ = self.send_costing(clock, 30000, status=None)
        self.assertEqual(endpoints.measure(send, "/x", count=200, max_s=10, clock=clock), [(None, 30000)])


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    seen = []

    def do_GET(self):
        Handler.seen.append((self.path, self.headers.get("Authorization"), id(self.connection)))
        if self.path.startswith("/Persons"):
            body = json.dumps({"Items": [{"Name": "Ann"}]}).encode()
        elif self.path.startswith("/Missing"):
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        else:
            body = b'{"Items": []}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


class Closing(Handler):
    """Answers, then drops the keep-alive connection without saying so (an idle-connection reaper)."""

    def do_GET(self):
        super().do_GET()
        self.close_connection = True


class Slow(Handler):
    def do_GET(self):
        time.sleep(1.0)
        with contextlib.suppress(BrokenPipeError, ConnectionResetError):   # the client gave up first
            super().do_GET()


class ClientTests(unittest.TestCase):
    def serve(self, handler):
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        return f"http://127.0.0.1:{server.server_address[1]}"

    def test_a_connection_the_server_closed_is_retried_once_on_a_fresh_one(self):
        client = endpoints.Client(self.serve(Closing), {}, timeout=5)
        self.assertEqual([client.get("/Items")[0] for _ in range(3)], [200, 200, 200])
        self.assertGreaterEqual(client.reconnects, 2)

    def test_a_request_past_its_timeout_has_no_status(self):
        client = endpoints.Client(self.serve(Slow), {}, timeout=0.2)
        status, ms = client.get("/Items")
        self.assertIsNone(status)
        self.assertLess(ms, 900)

    def test_a_server_that_is_not_there_has_no_status(self):
        url = self.serve(Handler)
        client = endpoints.Client(url.rsplit(":", 1)[0] + ":1", {}, timeout=1)
        self.assertEqual(client.get("/Items")[0], None)


class RunTests(unittest.TestCase):
    """`endpoints.run` against a local stand-in server: discovery, the warm-up, the loop, the file."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        Handler.seen = []
        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        self.ids = self.root / "ids.json"
        self.ids.write_text(json.dumps({"user": "u1", "browse_token": "tok", "movie": "m1"}))
        self.discover = self.root / "discover.toml"
        self.discover.write_text('[found.person]\npath = "/Persons"\npick = "Items.0.Name"\n'
                                 '[found.nobody]\npath = "/Missing"\npick = "Items.0.Name"\n')

    def run_pass(self, files, count=5):
        sets = write_set(self.root, files)
        out = self.root / "phase"
        out.mkdir(exist_ok=True)
        return endpoints.run(self.url, str(self.ids), str(out), sets=sets, count=count, max_s=10, timeout=5,
                             discover_path=str(self.discover), log=lambda *_: None)

    def test_every_endpoint_is_warmed_once_then_measured_alone_with_the_browse_role(self):
        doc = self.run_pass({"sweep/screens.jsonl": [get("a", "/Items", query={"userId": "{user}"}), get("b", "/Users/{user}/Views")]})
        paths = [p for p, _, _ in Handler.seen]
        self.assertEqual(paths[:2], ["/Items?userId=u1", "/Users/u1/Views"])   # the shared warm-up
        self.assertEqual(paths[2:], ["/Items?userId=u1"] * 5 + ["/Users/u1/Views"] * 5)
        self.assertTrue(all('DeviceId="bench-browse"' in a and 'Token="tok"' in a for _, a, _ in Handler.seen))
        self.assertEqual(len({c for _, _, c in Handler.seen}), 1, "one keep-alive connection")
        first = doc["endpoints"][0]
        self.assertEqual((first["id"], first["n"], first["errors"], first["gates"], first["target"]),
                         ("a", 5, 0, True, "/Items?userId=u1"))
        self.assertIsNotNone(first["p99"])
        saved = json.loads((self.root / "phase" / "endpoints.json").read_text())
        self.assertEqual(saved["params"]["count"], 5)

    def test_a_name_the_server_does_not_give_skips_only_the_endpoints_that_use_it(self):
        doc = self.run_pass({"sweep/contract.jsonl": [get("p", "/Persons/{found.person}"), get("n", "/Persons/{found.nobody}"), get("plain")]})
        by_id = {e["id"]: e for e in doc["endpoints"]}
        self.assertEqual(by_id["p"]["target"], "/Persons/Ann")
        self.assertIn("nobody", by_id["n"]["skipped"])
        self.assertNotIn("n", by_id["n"])
        self.assertEqual(by_id["plain"]["n"], 5)
        self.assertEqual(doc["found"], {"person": "Ann"})

    def test_an_error_answer_is_measured_and_counted(self):
        doc = self.run_pass({"sweep/contract.jsonl": [get("gone", "/Missing", expect_status=404), get("bad", "/Missing")]})
        by_id = {e["id"]: e for e in doc["endpoints"]}
        self.assertEqual((by_id["gone"]["errors"], by_id["gone"]["statuses"]), (0, {"404": 5}))
        self.assertEqual(by_id["bad"]["errors"], 5)

    def test_the_command_exits_one_when_the_server_answered_nothing(self):
        write_set(self.root, {"sweep/screens.jsonl": [get("a")]})
        out = self.root / "dead"
        out.mkdir()
        result = subprocess.run([sys.executable, ENDPOINTS_PY, "run", "http://127.0.0.1:1", str(self.ids), str(out),
                                 "--count", "2", "--timeout", "1", "--set", str(self.root / "sweep")],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(json.loads((out / "endpoints.json").read_text())["endpoints"][0]["statuses"], {"none": 2})

    def test_a_skipped_request_is_never_sent(self):
        self.run_pass({"sweep/contract.jsonl": [get("GetVideoStream", "/Videos/{movie}/stream"), get("plain")]})
        self.assertFalse([p for p, _, _ in Handler.seen if "stream" in p])


def doc_of(p50s, target="/Items", statuses=None, gates=True, eid="home", p95=None):
    """An endpoints.json of one run with one endpoint."""
    return {"params": {"count": 200, "max_s": 10, "timeout_s": 10, "sets": list(endpoints.SETS),
                       "gated_files": list(endpoints.GATED_FILES)}, "endpoints": [
        {"id": eid, "file": "sweep/screens.jsonl", "gates": gates, "target": target, "n": 200, "errors": 0,
         "statuses": statuses or {"200": 200}, "p50": p50s, "p95": p95 or p50s * 2, "p99": p50s * 3}]}


class VerdictTests(unittest.TestCase):
    gate_rows = {"endpoints": BAND}

    def rows(self, a, b, **kw):
        return gate.endpoint_verdicts([doc_of(v, **kw) for v in a], [doc_of(v, **kw) for v in b], self.gate_rows)

    def test_a_slower_candidate_regresses_past_both_thresholds_in_two_of_three_pairs(self):
        row = self.rows([2.0, 2.1, 2.0], [3.0, 2.0, 3.1])["endpoints/home"]
        self.assertEqual(row["verdict"], "regressed")
        self.assertEqual((row["a_median"], row["b_median"], row["slower"]), (2.0, 3.0, 2))
        self.assertEqual((row["a_p95"], row["b_p99"]), (4.0, 9.0))

    def test_under_the_absolute_threshold_is_within_noise_however_large_the_ratio(self):
        self.assertEqual(self.rows([0.2] * 3, [0.6] * 3)["endpoints/home"]["verdict"], "within noise")

    def test_under_the_relative_threshold_is_within_noise(self):
        self.assertEqual(self.rows([10.0] * 3, [11.0] * 3)["endpoints/home"]["verdict"], "within noise")

    def test_slower_in_only_one_pair_is_within_noise(self):
        self.assertEqual(self.rows([2.0, 2.0, 2.0], [3.0, 1.9, 1.9])["endpoints/home"]["verdict"], "within noise")

    def test_a_faster_candidate_improves(self):
        self.assertEqual(self.rows([3.0] * 3, [2.0] * 3)["endpoints/home"]["verdict"], "improved")

    def test_a_self_ab_of_one_image_reports_no_regression(self):
        rows = self.rows([2.00, 2.03, 1.98], [2.01, 1.99, 2.02])
        self.assertEqual(gate.blocking(rows), [])

    def test_a_reported_endpoint_never_blocks(self):
        rows = self.rows([2.0] * 3, [9.0] * 3, gates=False)
        self.assertEqual(rows["endpoints/home"]["verdict"], "regressed")
        self.assertNotIn("endpoints/home", gate.blocking(rows))

    def test_a_gated_endpoint_a_candidate_run_could_not_fill_blocks(self):
        lost = {"endpoints": [{"id": "home", "file": "sweep/screens.jsonl", "gates": True,
                               "skipped": "cannot be filled: no `found.person`"}]}
        for b in ([doc_of(2.0), doc_of(2.0), lost], [lost, doc_of(2.0), doc_of(2.0)]):
            with self.subTest(b):
                rows = gate.endpoint_verdicts([doc_of(2.0)] * 3, b, self.gate_rows)
                self.assertEqual(gate.blocking(rows), ["endpoints/home"])

    def test_a_session_that_measured_nothing_blocks(self):
        for docs in ([None] * 3, [{"endpoints": []}] * 3):
            with self.subTest(docs):
                rows = gate.endpoint_verdicts(docs, docs, self.gate_rows)
                self.assertEqual(gate.blocking(rows), ["endpoints/*"])

    def test_a_candidate_answering_another_status_cannot_be_judged_and_blocks_when_gated(self):
        a = [doc_of(2.0) for _ in range(3)]
        b = [doc_of(2.0, statuses={"500": 200}) for _ in range(3)]
        row = gate.endpoint_verdicts(a, b, self.gate_rows)["endpoints/home"]
        self.assertEqual(row["verdict"], "not comparable")
        self.assertIn("500", row["reason"])
        self.assertEqual(gate.blocking({"endpoints/home": row}), ["endpoints/home"])

    def test_mixed_statuses_within_a_run_cannot_be_judged(self):
        b = [doc_of(2.0, statuses={"200": 150, "none": 50}) for _ in range(3)]
        row = gate.endpoint_verdicts([doc_of(2.0)] * 3, b, self.gate_rows)["endpoints/home"]
        self.assertEqual(row["verdict"], "not comparable")

    def test_a_request_filled_differently_cannot_be_judged(self):
        a = [doc_of(2.0) for _ in range(3)]
        b = [doc_of(2.0, target="/Items?other") for _ in range(3)]
        self.assertIn("asked differently", gate.endpoint_verdicts(a, b, self.gate_rows)["endpoints/home"]["reason"])

    def test_an_endpoint_missing_from_a_run_cannot_be_judged(self):
        a = [doc_of(2.0) for _ in range(3)]
        b = [doc_of(2.0), doc_of(2.0), {"endpoints": []}]
        self.assertEqual(gate.endpoint_verdicts(a, b, self.gate_rows)["endpoints/home"]["verdict"], "not comparable")

    def test_a_skipped_endpoint_is_listed_and_never_blocks(self):
        skipped = {"endpoints": [{"id": "GetVideoStream", "file": "sweep/contract.jsonl", "gates": False, "skipped": "media"}]}
        rows = gate.endpoint_verdicts([skipped] * 3, [skipped] * 3, self.gate_rows)
        self.assertEqual(rows["endpoints/GetVideoStream"]["verdict"], "not comparable")
        self.assertNotIn("endpoints/GetVideoStream", gate.blocking(rows))

    def test_a_per_cell_band_replaces_the_row(self):
        rows = gate.endpoint_verdicts([doc_of(2.0)] * 3, [doc_of(3.0)] * 3,
                                      {"endpoints": BAND, "cells": {"endpoints/home": {"relative_pct": 80, "absolute": 5}}})
        self.assertEqual(rows["endpoints/home"]["verdict"], "within noise")


class SessionTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def session(self, p50s):
        out = self.root / f"gate-endpoints-{len(list(self.root.iterdir()))}"
        plan = {"mode": "endpoints", "name": out.name, "date": "2026-10-24T00:00:00+00:00",
                "schedule": gate.schedule("endpoints", {"a": "old", "b": "new"}, len(p50s) // 2),
                "images": {"a": {"ref": "old", "id": "sha256:old"}, "b": {"ref": "new", "id": "sha256:new"}}}
        out.mkdir()
        (out / "gate.json").write_text(json.dumps(plan))
        for (name, _, _), v in zip(plan["schedule"], p50s):
            d = out / name / "ferrofin"
            d.mkdir(parents=True)
            doc = doc_of(v)
            doc["endpoints"].append({**doc_of(v * 4, gates=False, eid="GetItems.isHd.true")["endpoints"][0]})
            (d / "endpoints.json").write_text(json.dumps(doc))
        return out

    def test_the_schedule_is_three_interleaved_pairs_and_dry_run_starts_nothing(self):
        out = self.root / "session"
        result = subprocess.run([sys.executable, GATE_PY, "endpoints", "old", "new", "--dry-run", "--out", str(out)],
                                capture_output=True, text=True, env=dict(os.environ, PATH=""))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.count("--only endpoints"), 6)
        self.assertIn("06-b  b  new", result.stdout)
        self.assertFalse(out.exists())

    def test_render_blocks_on_a_gated_regression_and_lists_every_endpoint_with_its_percentiles(self):
        out = self.session([2.0, 3.0, 2.0, 3.0, 2.0, 3.0])
        md, block = gate.render(str(out))
        self.assertEqual(block, ["endpoints/home"])
        page = (out / "report.html").read_text()
        for text in ("endpoints/home", "endpoints/GetItems.isHd.true", "p95", "p99", "regressed"):
            self.assertIn(text, page)
            self.assertIn(text, md)
        self.assertEqual(json.loads((out / "verdict.json").read_text())["blocking"], ["endpoints/home"])

    def test_a_self_ab_passes_and_reaches_the_candidate_runs_in_the_viewer(self):
        out = self.session([2.0, 2.01, 2.02, 1.99, 2.0, 2.0])
        md, block = gate.render(str(out))
        self.assertEqual(block, [], md)
        self.assertIn("PASS", md)
        cells = report.load_verdicts([str(out / "02-b")])
        self.assertEqual([c["cell"] for c in cells], ["endpoints/home"])  # only gated cells reach the viewer


class RunReportTests(unittest.TestCase):
    def test_the_run_report_shows_both_servers_percentiles_per_endpoint(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = Path(tmp) / "run"
            for server, v in (("jellyfin", 8.0), ("ferrofin", 2.0)):
                (run / server).mkdir(parents=True)
                (run / server / "endpoints.json").write_text(json.dumps(doc_of(v)))
            m = {"runs": [str(run)], "servers": report.SERVERS}
            for as_html in (False, True):
                page = report.endpoints_table(m, as_html)
                self.assertIn("home", page)
                self.assertIn("8", page)
                self.assertIn("4.0× faster", page)
            self.assertEqual(report.endpoints_table({"runs": [str(Path(tmp) / "none")], "servers": report.SERVERS}), "")

    def test_the_run_report_flags_an_endpoint_the_servers_answered_differently(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = Path(tmp) / "run"
            for server, st in (("jellyfin", {"500": 200}), ("ferrofin", {"200": 200})):
                (run / server).mkdir(parents=True)
                (run / server / "endpoints.json").write_text(json.dumps(doc_of(2.0, statuses=st)))
            page = report.endpoints_table({"runs": [str(run)], "servers": report.SERVERS})
            self.assertIn("Ferrofin answered 200, Jellyfin reference 500", page)


class RunShArgumentTests(unittest.TestCase):
    def run_sh(self, *args, **env):
        return subprocess.run(["bash", RUN_SH, "--testdata", "/nonexistent-testdata", "--out", "/nonexistent-out", *args],
                              env=dict(os.environ, **env), capture_output=True, text=True)

    def test_endpoints_is_a_known_phase_for_both_servers(self):
        for servers in ([], ["--servers", "ferrofin"], ["--servers", "jellyfin"]):
            with self.subTest(servers):
                self.assertIn("build the test data", self.run_sh("--only", "endpoints", *servers).stderr)

    def test_the_endpoint_tunables_must_be_positive_integers(self):
        for name in ("ENDPOINT_COUNT", "ENDPOINT_MAX_S"):
            with self.subTest(name):
                result = self.run_sh("--only", "counts", **{name: "0"})
                self.assertEqual(result.returncode, 2)
                self.assertIn(f"{name} must be a positive integer", result.stderr)

    def test_plan_prints_the_schedule_without_a_server(self):
        result = subprocess.run([sys.executable, ENDPOINTS_PY, "plan"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("gate", result.stdout)
        self.assertIn("skipped GetVideoStream", result.stdout)
        bound = subprocess.run([sys.executable, ENDPOINTS_PY, "plan", "--bound", "--count", "200", "--max-s", "10",
                                "--timeout", "10"], capture_output=True, text=True)
        self.assertEqual(bound.returncode, 0, bound.stderr)
        self.assertGreater(int(bound.stdout), 10 * 2000)


if __name__ == "__main__":
    unittest.main()
