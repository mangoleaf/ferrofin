"""gates/report.py: the combined page of a gates session, built from gate folders in the shapes the gates
write them (adoption's own recorder, bench's phase states, gate.py's gated cells) (stdlib)."""

import importlib.util
import json
import os
import sys
import tempfile
import unittest
from datetime import datetime
from pathlib import Path

GATES = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(GATES))
import session  # noqa: E402

spec = importlib.util.spec_from_file_location("gates_report", GATES / "report.py")
report = importlib.util.module_from_spec(spec)
spec.loader.exec_module(report)
adoption_report = report.gate_module("adoption/adoption_report.py")
verify_report = report.gate_module("verify/verify_report.py")
bench_report = report.gate_module("bench/report.py")
bench_gate = report.gate_module("bench/gate.py")
parity_diff = report.gate_module("parity/diff.py")
TAG = "0123456789abcdef0123456789abcdef"   # an image tag, as a key in an item's ImageBlurHashes


class SessionReportTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)

    def session(self, gates, statuses, previous=None):
        s = Path(session.new(os.path.join(self.tmp.name, "runs"), list(gates), "release/v9", "c0ffee1234567890", False,
                             "ferrofin:v9", "sha256:" + "a" * 64, "jellyfin/jellyfin@sha256:pin", previous,
                             previous and "sha256:" + "b" * 64, when=datetime(2026, 10, 24, 12, 0)))
        for gate, (status, code) in statuses.items():
            session.gate(str(s), gate, "running")
            session.gate(str(s), gate, status, code)
            (s / f"{gate}.log").write_text("what it printed\n")
        return s

    def write(self, path, data):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(data if isinstance(data, str) else json.dumps(data))

    def verify_run(self, d, raw, log=True):
        """What gates/verify/run.sh leaves: verify.json, then result.json and report.html from verify's own code."""
        self.write(d / "verify.json", raw)
        if log:
            self.write(d / "server.log", "the server's log\n")
        doc = verify_report.build(str(d), "ferrofin:v9", "sha256:" + "a" * 64, "c0ffee1234567890", "2026-10-24T12:00:00+00:00")
        verify_report.result.write(str(d), doc)
        verify_report.write_report(str(d), doc)

    def bench_run(self, d, phases):
        """What gates/bench/run.sh leaves: result.json, its verdict and counts from the benchmark's own
        reading of each server's phases.json."""
        verdict, counts = bench_report.phase_verdict({"phases": {k: [p] for k, p in phases.items()}, "meta": {"phase_schema": 1}})
        d.mkdir(parents=True, exist_ok=True)
        bench_report.gate_result().write(str(d), bench_report.gate_result().envelope(
            "bench", verdict, counts, {"image": "ferrofin:v9"}, None, "2026-10-24T13:00:00+00:00",
            {"servers": {k: k.title() for k in phases}}))

    def parity_run(self, d, summary):
        """What gates/parity/diff.py writes for a run's results.json `summary`: results.json and its result.json."""
        self.write(d / "results.json", summary)
        parity_diff.gate_result().write(str(d), parity_diff.result_doc(summary))

    def ab_session(self, d, verdicts, blocking):
        """What gate.py ab leaves: the session's result.json, from gate.py's own session_result."""
        plan = {"mode": "ab", "name": "gate", "date": "2026-10-24T12:00:00+00:00",
                "schedule": [["01-a", "a", "old"], ["02-b", "b", "new"]],
                "images": {"a": {"ref": "ferrofin:v8", "id": "sha256:a"}, "b": {"ref": "ferrofin:v9", "id": "sha256:b"}}}
        d.mkdir(parents=True, exist_ok=True)
        bench_report.gate_result().write(str(d), bench_gate.session_result(str(d), plan, {"verdicts": verdicts}, blocking))

    def adoption_run(self, d, rows, unfinished=()):
        """What gates/adoption/run.sh records: a begin per fixture, its checks, then its row."""
        recorder = adoption_report.Recorder(str(d))
        recorder.start("gates/adoption/run.sh", "ferrofin:v9", "12.2")
        for fixture, verdict, failed_check in rows:
            recorder.begin(fixture, "path", "12.2.0")
            recorder.check(fixture, "after adoption", "boot", "FAIL" if failed_check else "PASS")
            recorder.row(fixture, "path", "12.2.0", verdict)
        for fixture in unfinished:   # killed while on it
            recorder.begin(fixture, "path", "12.2.0")
        self.adoption_result(d)

    def adoption_result(self, d):
        """The result.json adoption's own build writes from what was recorded (its page is left to each test)."""
        adoption_report.gate_result().write(str(d), adoption_report.result_doc(*adoption_report.load(str(d))))

    def every_gate(self, s):
        self.verify_run(s / "verify", {"exit": 1, "libraries": {
            "movies": {"rows": [{"row": 1, "result": "PASS"}, {"row": 2, "result": "PASS"}, {"row": 13, "result": "SKIP"}]},
            "tvshows": {"rows": [{"row": 1, "result": "FAIL"}]}}})
        self.write(s / "verify/scan-behaviour-movies.txt", "rows\n")
        self.adoption_run(s / "adoption", [("a", "PASS", False), ("b", "PASS", False), ("c", "SKIP", False)])
        self.write(s / "adoption/report.html", "<p>adoption</p>")
        new = lambda path, kind, filed=None: {"path": path, "kind": kind, "ledger": None, "filed": filed}
        self.parity_run(s / "parity", {"verdict": "new differences", "reason": "2 requests differ", "planned": 40, "requests": [
            {"target": "/Items/1", "state": "differences",
             "differences": [new(f"ImageBlurHashes.Primary.{TAG}", "value"), new("B", "missing")]},
            {"target": "/Items/2", "state": "differences",
             "differences": [new(f"ImageBlurHashes.Primary.{'f' * 32}", "value"), new("C", "value", "gl-1")]},
            *[{"target": "/Users/x", "state": "match", "differences": []}] * 36]})
        self.write(s / "parity/report.html", "<p>parity</p>")
        self.bench_run(s / "bench", {"ferrofin": {"counts": {"status": "completed"}, "loaded": {"status": "failed"},
                                                  "ttfs": {"status": "skipped"}}, "jellyfin": {}})
        self.write(s / "bench/report.html", "<p>bench</p>")

    def rows(self, s):
        return {r["gate"]: r for r in report.summarise(str(s))[1]}

    def test_a_session_of_every_gate_lists_each_with_its_counts_and_the_build_it_judged(self):
        s = self.session(report.GATES, {"verify": ("failed", 1), "adoption": ("passed", 0),
                                        "parity": ("failed", 1), "bench": ("failed", 1)})
        self.every_gate(s)
        rows = self.rows(s)
        self.assertEqual(rows["verify"]["counts"], [("checks passed", 2), ("failed", 1), ("skipped", 1)])
        self.assertEqual((rows["adoption"]["verdict"], rows["adoption"]["counts"]),
                         ("passed", [("rows passed", 2), ("failed", 0), ("unfinished", 0), ("skipped", 1)]))
        self.assertEqual(rows["parity"]["verdict"], "new differences: 2 requests differ")
        # two image tags of one field are one group, as `diff.py findings` files them
        self.assertEqual(rows["parity"]["counts"], [("new differences", 4), ("groups not filed", 2),
                                                    ("requests sent", 38), ("planned", 40)])
        self.assertEqual((rows["bench"]["verdict"], rows["bench"]["counts"]),
                         ("1 phase failed", [("phases completed", 1), ("failed", 1), ("unfinished", 0), ("skipped", 1)]))
        self.assertEqual({g: r["problem"] for g, r in rows.items()}, dict.fromkeys(report.GATES))
        page = Path(report.write(str(s))).read_text(encoding="utf-8")
        for text in ("Gates session release-v9-20261024-1200", "c0ffee123456", "ferrofin:v9 (sha256:aaaaaaaaaaaa)",
                     "jellyfin/jellyfin@sha256:pin",
                     "<a href='adoption/report.html'>adoption report</a>", "<a href='verify.log'>log</a>",
                     "<b><a href='verify/report.html'>verify report</a></b><br><span class='meta'>"
                     "<a href='verify/scan-behaviour-movies.txt'>movies rows</a> · "):
            self.assertIn(text, page)
        self.assertNotIn(self.tmp.name, page)   # every link is relative: the page opens from anywhere
        self.assertIn("| parity | failed (exit 1) | new differences: 2 requests differ | new differences 4, groups not filed 2, "
                      "requests sent 38, planned 40 |", (s / "report.md").read_text(encoding="utf-8"))

    def test_a_single_gate_session_shows_the_others_as_not_run(self):
        s = self.session(["adoption"], {"adoption": ("passed", 0)})
        self.adoption_run(s / "adoption", [("a", "PASS", False)])
        self.write(s / "adoption/report.html", "<p>adoption</p>")
        rows = self.rows(s)
        self.assertEqual(list(rows), list(report.GATES))
        self.assertEqual({g: r["verdict"] for g, r in rows.items()},
                         {"verify": "not run", "adoption": "passed", "parity": "not run", "bench": "not run"})
        self.assertIn("not requested", Path(report.write(str(s))).read_text(encoding="utf-8"))

    def test_adoption_is_judged_by_its_own_rules_a_killed_fixture_and_a_failed_check_fail_it(self):
        s = self.session(["adoption"], {"adoption": ("failed", 143)})
        self.adoption_run(s / "adoption", [("a", "PASS", False), ("b", "PASS", True)], unfinished=["c"])
        with open(s / "adoption/results.jsonl", "a") as f:
            f.write('{"event": "row", "fixt')   # the line a kill cut short
        self.adoption_result(s / "adoption")
        row = self.rows(s)["adoption"]
        self.assertEqual((row["verdict"], row["counts"]),
                         ("failed", [("rows passed", 1), ("failed", 1), ("unfinished", 1), ("skipped", 0)]))
        self.assertEqual(row["problem"], "1 unreadable line in results.jsonl skipped; "
                                         "no report.html: the gate ended before writing its report")

    def test_bench_counts_only_completed_phases_and_a_run_cut_short_is_incomplete(self):
        s = self.session(["bench"], {"bench": ("failed", 143)})
        self.assertEqual(self.rows(s)["bench"]["problem"], "no result.json: the run ended before writing its report and result; "
                                                           "no report.html: the gate ended before writing its report")
        self.bench_run(s / "bench", {"ferrofin": {"counts": {"status": "completed"}, "loaded": {"status": "running"},
                                                  "stress": {"status": "pending"}, "ttfs": {"status": "pending"}}})
        row = self.rows(s)["bench"]
        self.assertEqual((row["verdict"], row["counts"]),
                         ("incomplete", [("phases completed", 1), ("failed", 0), ("unfinished", 3), ("skipped", 0)]))
        # the Jellyfin half never started: no results of it at all
        self.assertEqual(row["problem"], "no results for jellyfin: its run never started; "
                                         "no report.html: the gate ended before writing its report")

    def test_an_adoption_run_from_before_result_json_says_how_to_add_one(self):
        s = self.session(["adoption"], {"adoption": ("passed", 0)})
        self.adoption_run(s / "adoption", [("a", "PASS", False)])
        (s / "adoption/result.json").unlink()
        self.assertIn("a run from before adoption wrote one (python3 gates/adoption/adoption_report.py build DIR",
                      self.rows(s)["adoption"]["problem"])

    def test_a_bench_run_from_before_result_json_says_how_to_add_one(self):
        s = self.session(["bench"], {"bench": ("passed", 0)})
        self.write(s / "bench/report.md", "# tables\n")
        self.assertIn("a run from before the benchmark wrote one (python3 gates/bench/report.py --html DIR",
                      self.rows(s)["bench"]["problem"])
        s = self.session(["bench"], {"bench": ("passed", 0)}, previous="ferrofin:v8")
        self.write(s / "bench/verdict.json", {"verdicts": {}})
        self.assertIn("(python3 gates/bench/gate.py render DIR writes it)", self.rows(s)["bench"]["problem"])

    def test_a_bench_session_shows_the_readme_graphs_of_its_run(self):
        from test_publish import bench_result   # a Jellyfin-comparison run's result.json
        s = self.session(["bench"], {"bench": ("passed", 0)})
        (s / "bench").mkdir()
        bench_report.gate_result().write(str(s / "bench"), bench_result())
        page = Path(report.write(str(s))).read_text(encoding="utf-8")
        self.assertIn("<h2>Benchmark against Jellyfin</h2>", page)
        self.assertEqual(page.count("<div class='graph light'><svg"), page.count("<div class='graph dark'><svg"))
        self.assertIn("@media (prefers-color-scheme:dark){.graph.light{display:none}", page)
        self.assertNotIn("<img", page)   # inline: the page stays self-contained
        doc = json.loads((s / "bench/result.json").read_text())
        del doc["detail"]["servers"]["jellyfin"]   # a Ferrofin-only run has nothing to graph against
        self.write(s / "bench/result.json", doc)
        self.assertNotIn("Benchmark against Jellyfin", Path(report.write(str(s))).read_text(encoding="utf-8"))

    def test_a_parity_session_shows_the_readme_parity_overview(self):
        from test_publish import parity_requests
        s = self.session(["parity"], {"parity": ("failed", 1)})
        self.parity_run(s / "parity", {"verdict": "new differences", "reason": "1 new", "planned": 12,
                                       "requests": parity_requests(), "run": {"oracle": {"version": "12.2.0"}}})
        page = Path(report.write(str(s))).read_text(encoding="utf-8")
        self.assertIn("<h2>Parity</h2>", page)
        self.assertEqual(page.count("<div class='graph light'><svg"), 1)
        self.assertIn("Answering jellyfin-web like Jellyfin 12.2.0", page)
        self.assertIn("Ferrofin (commit c0ffee12) against Jellyfin 12.2.0", page)
        self.parity_run(s / "parity", {"verdict": "run failed", "reason": "ferrofin did not start", "planned": 0, "requests": []})
        page = Path(report.write(str(s))).read_text(encoding="utf-8")
        self.assertNotIn("<h2>Parity</h2>", page)   # nothing compared: the gate's row says why, no graph and no error
        self.assertNotIn("could not be drawn", page)

    def test_an_adoption_session_shows_the_readme_adoption_grid(self):
        s = self.session(["adoption"], {"adoption": ("failed", 1)})
        self.adoption_run(s / "adoption", [("jellyfin-12.2-fresh", "PASS", False), ("jellyfin-10.11.11-messy", "PASS", True)])
        page = Path(report.write(str(s))).read_text(encoding="utf-8")
        self.assertIn("<h2>Adoption</h2>", page)
        self.assertEqual(page.count("<div class='graph light'><svg"), 1)
        self.assertIn(">failed<", page)
        self.assertIn("12.2-fresh", page)
        self.assertIn("Ferrofin (commit c0ffee12), gates session", page)

    def test_an_ab_gate_session_counts_its_gated_cells_and_what_blocks(self):
        s = self.session(["bench"], {"bench": ("failed", 1)}, previous="ferrofin:v8")
        self.assertEqual(self.rows(s)["bench"]["problem"], "no result.json: the A/B gate ended before it judged its runs; "
                                                           "no report.html: the gate ended before writing its report")
        self.ab_session(s / "bench", {"x": {"verdict": "regressed", "gated": True}, "y": {"verdict": "within noise", "gated": True},
                                      "z": {"verdict": "regressed", "gated": False}}, ["x"])
        self.write(s / "bench/report.html", "<p>gate</p>")
        row = self.rows(s)["bench"]
        self.assertEqual((row["verdict"], row["links"]), ("blocked by 1 cell", [("A/B gate report", "bench/report.html"),
                                                                              ("log", "bench.log")]))
        self.assertIn("<dt>previous release</dt><dd>ferrofin:v8</dd>", Path(report.write(str(s))).read_text(encoding="utf-8"))
        self.assertEqual(row["counts"], [("gated cells regressed", 1), ("not comparable", 0), ("improved", 0), ("within noise", 1)])

    def test_the_oracle_line_names_the_release_the_branch_pins_and_a_newer_one(self):
        s = self.session(["bench"], {"bench": ("passed", 0)}, previous="ferrofin:v8")
        self.assertEqual(dict(report.facts(report.summarise(str(s))[0]))["Jellyfin oracle"],
                         "jellyfin/jellyfin@sha256:pin")   # a session made before the oracle carried its tag
        note = "measured against Jellyfin v12.2, the release this branch pins; v12.3 is out"
        record = session.load(str(s))
        record["oracle"].update(tag="v12.2", note=note)
        session.save(str(s), record)
        line = f"v12.2 (jellyfin/jellyfin@sha256:pin) — {note}"
        self.assertIn(f"<dt>Jellyfin oracle</dt><dd>{line}</dd>", Path(report.write(str(s))).read_text(encoding="utf-8"))
        self.assertIn(f"- **Jellyfin oracle:** {line}", (s / "report.md").read_text(encoding="utf-8"))

    def test_missing_corrupt_and_misshapen_results_are_shown_as_such(self):
        s = self.session(report.GATES, {g: ("failed", 1) for g in report.GATES})
        self.write(s / "parity/result.json", "{not json")
        self.verify_run(s / "verify", {"exit": 3, "libraries": {"movies": None, "tvshows": None}})
        self.bench_run(s / "bench", {})
        doc = json.loads((s / "bench/result.json").read_text())
        self.write(s / "bench/result.json", {**doc, "detail": {"servers": 5}})
        rows = self.rows(s)
        self.assertEqual((rows["verify"]["verdict"], rows["verify"]["problem"]),
                         ("nothing verified", "no rows from the movies and tvshows libraries: the check ended before them "
                                              "(the run's exit 3, see the log)"))
        self.assertEqual(rows["adoption"]["problem"], "no result.json: the gate ended before it recorded anything; "
                                                      "no report.html: the gate ended before writing its report")
        self.assertIn("result.json cannot be read", rows["parity"]["problem"])
        self.assertIn("its result is not shaped as expected (TypeError", rows["bench"]["problem"])
        (s / "parity/result.json").unlink()
        self.write(s / "parity/results.json", {"verdict": "pass", "planned": 1, "requests": []})
        self.assertEqual(self.rows(s)["parity"]["problem"],
                         "no result.json: a run from before parity wrote one (python3 gates/parity/diff.py result --out DIR "
                         "writes it); no report.html: the gate ended before writing its report")

    def test_a_running_gate_one_not_reached_and_one_never_started_are_shown(self):
        s = self.session(["verify", "adoption", "bench"], {})
        session.gate(str(s), "verify", "running")
        session.gate(str(s), "adoption", "failed", note="the image ferrofin:v9 moved off sha256:aaa")
        rows = self.rows(s)
        self.assertEqual({g: r["verdict"] for g, r in rows.items() if g != "parity"},
                         {"verify": "running", "adoption": "not started", "bench": "not run"})
        self.assertEqual((report.status_word(rows["adoption"]), rows["adoption"]["problem"]),
                         ("failed, the image ferrofin:v9 moved off sha256:aaa", None))
        session.end(str(s))
        rows = self.rows(s)
        self.assertEqual(report.status_word(rows["verify"]), "failed, interrupted")
        self.assertEqual(rows["verify"]["problem"], "no result.json: the gate ended before writing its result; "
                                                    "no report.html: the gate ended before writing its report")

    def test_a_verify_run_stopped_during_its_second_library_is_incomplete(self):
        s = self.session(["verify"], {"verify": ("failed", 143)})
        self.verify_run(s / "verify", {"exit": 143, "libraries": {
            "movies": {"rows": [{"row": 1, "result": "PASS"}]}, "tvshows": None}})
        row = self.rows(s)["verify"]
        self.assertEqual((row["verdict"], row["problem"], row["bad"]),
                         ("incomplete", "no rows from the tvshows library: the check ended before them "
                                        "(the run's exit 143, see the log)", True))

    def test_verify_links_its_report_first_then_its_raw_files_and_log(self):
        s = self.session(["verify"], {"verify": ("passed", 0)})
        self.verify_run(s / "verify", {"exit": 0, "libraries": {"movies": {"rows": [{"row": 1, "result": "PASS"}]},
                                                                "tvshows": {"rows": [{"row": 1, "result": "PASS"}]}}})
        self.write(s / "verify/scan-behaviour-movies.txt", "rows\n")
        row = self.rows(s)["verify"]
        self.assertEqual((row["verdict"], row["problem"]), ("passed", None))
        self.assertEqual(row["links"], [("verify report", "verify/report.html"), ("movies rows", "verify/scan-behaviour-movies.txt"),
                                        ("result.json", "verify/result.json"), ("verify.json", "verify/verify.json"),
                                        ("server log", "verify/server.log"), ("log", "verify.log")])

    def test_a_verify_run_from_before_result_json_says_how_to_add_one(self):
        s = self.session(["verify"], {"verify": ("passed", 0)})
        self.write(s / "verify/verify.json", {"exit": 0, "libraries": {"movies": None, "tvshows": None}})
        self.assertIn("no result.json: a run from before verify wrote one (python3 gates/verify/verify_report.py",
                      self.rows(s)["verify"]["problem"])

    def test_a_verify_result_that_is_not_a_valid_envelope_is_unreadable(self):
        s = self.session(["verify"], {"verify": ("failed", 1)})
        self.write(s / "verify/result.json", {"schema": 1, "gate": "verify"})
        self.assertIn("result.json is not a valid result: missing", self.rows(s)["verify"]["problem"])
        self.verify_run(s / "verify", {"exit": 0, "libraries": {"movies": None, "tvshows": None}})
        other = {**json.loads((s / "verify/result.json").read_text()), "gate": "parity"}
        self.write(s / "verify/result.json", other)
        self.assertEqual(self.rows(s)["verify"]["problem"], "result.json is parity's result, not verify's")
        self.write(s / "verify/result.json", "{cut")
        self.assertIn("result.json cannot be read", self.rows(s)["verify"]["problem"])

    def test_a_page_on_disk_is_linked_even_when_the_result_beside_it_cannot_be_read(self):
        s = self.session(["parity"], {"parity": ("failed", 1)})
        self.write(s / "parity/result.json", "{cut")
        self.write(s / "parity/report.html", "<p>parity</p>")
        row = self.rows(s)["parity"]
        self.assertEqual(row["links"], [("parity report", "parity/report.html"), ("log", "parity.log")])

    def test_a_gate_that_exited_0_but_whose_result_failed_is_coloured_bad(self):
        s = self.session(["adoption"], {"adoption": ("passed", 0)})
        self.adoption_run(s / "adoption", [("a", "PASS", True)])   # a failed check under a PASS row
        self.write(s / "adoption/report.html", "<p>adoption</p>")
        self.assertEqual(self.rows(s)["adoption"]["verdict"], "failed")
        self.assertIn("<tr class='bad'><th>adoption</th>", Path(report.write(str(s))).read_text(encoding="utf-8"))

    def test_free_text_is_escaped_in_the_page_and_kept_in_its_markdown_cell(self):
        s = self.session(["parity"], {"parity": ("failed", 1)})
        self.parity_run(s / "parity", {"verdict": "run failed", "reason": "<b>a | b\nc</b>", "planned": 0, "requests": []})
        self.write(s / "parity/report.html", "<p>parity</p>")
        page = Path(report.write(str(s))).read_text(encoding="utf-8")
        self.assertIn("run failed: &lt;b&gt;a | b\nc&lt;/b&gt;", page)
        self.assertIn("| run failed: <b>a \\| b c</b> |", (s / "report.md").read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
