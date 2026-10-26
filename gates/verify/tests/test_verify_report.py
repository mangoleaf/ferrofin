"""gates/verify/verify_report.py: verify's result.json and report.html from a fixed verify.json (stdlib)."""

import contextlib
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path

VERIFY = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(VERIFY))
import verify_report  # noqa: E402

CHECKS = [{"what": "created", "observed": 3, "op": "eq", "expected": 3, "ok": True},
          {"what": "updated", "observed": 1, "op": "eq", "expected": 0, "ok": False},
          {"what": "Overview", "observed": "old <b>", "op": "eq", "expected": "new", "ok": False},
          {"what": "the edit was refused", "observed": None, "op": None, "expected": None, "ok": False}]
RAW = {"exit": 1, "libraries": {
    "movies": {"rows": [{"row": 1, "scenario": "first scan of new items", "result": "PASS", "detail": "created=3",
                         "checks": CHECKS[:1]},
                        {"row": 2, "scenario": "rescan, nothing changed", "result": "FAIL",
                         "detail": "updated 1 (want = 0) | created=0", "checks": CHECKS},
                        {"row": 13, "scenario": "a webhook scan", "result": "SKIP", "detail": "needs a TV library", "checks": []}],
               "summary": {"pass": 1, "fail": 1, "skip": 1}},
    "tvshows": None}}


class VerifyReportTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.run_dir = Path(tmp.name)

    def write_run(self, raw, log=True):
        (self.run_dir / "verify.json").write_text(json.dumps(raw))
        if log:
            (self.run_dir / "server.log").write_text("log\n")
        return self.run_dir

    def test_the_result_is_a_valid_envelope_with_every_row_and_check(self):
        d = self.write_run(RAW)
        with contextlib.redirect_stdout(io.StringIO()) as out:
            self.assertEqual(verify_report.main([str(d), "--image", "ferrofin:v9", "--image-id", "sha256:abc",
                                                 "--commit", "c0ffee", "--started", "2026-10-25T12:00:00+00:00"]), 0)
        self.assertEqual(out.getvalue().strip(), str(d / "report.html"))
        self.assertTrue((d / "report.html").is_file())
        doc = verify_report.result.read(str(d), "verify")
        self.assertEqual((doc["verdict"], doc["counts"]), ("failed", {"pass": 1, "fail": 1, "skip": 1}))
        self.assertEqual({k: doc["source"][k] for k in ("commit", "image", "image_digest", "oracle", "fixture")},
                         {"commit": "c0ffee", "image": "ferrofin:v9", "image_digest": "sha256:abc", "oracle": None, "fixture": None})
        self.assertEqual(set(doc["source"]["host_class"]), {"cores", "memory_gib"})
        self.assertEqual(doc["started"], "2026-10-25T12:00:00+00:00")
        self.assertEqual(doc["detail"]["libraries"]["movies"]["rows"][1]["checks"], CHECKS)
        self.assertIsNone(doc["detail"]["libraries"]["tvshows"])

    def test_the_verdict_follows_the_rows(self):
        row = lambda result: {"rows": [{"row": 1, "result": result}]}
        for libraries, verdict in (({"movies": row("PASS"), "tvshows": row("SKIP")}, "passed"),
                                   ({"movies": row("PASS"), "tvshows": None}, "incomplete"),
                                   ({"movies": row("FAIL"), "tvshows": None}, "failed"),
                                   ({"movies": None, "tvshows": None}, "nothing verified")):
            with self.subTest(verdict):
                self.assertEqual(verify_report.build(str(self.write_run({"exit": 0, "libraries": libraries})), "x")["verdict"],
                                 verdict)

    def test_rows_from_before_the_checks_were_recorded_get_none(self):
        doc = verify_report.build(str(self.write_run({"exit": 0, "libraries": {"movies": {"rows": [{"row": 1, "result": "PASS"}]}}})), "x")
        self.assertEqual(doc["detail"]["libraries"]["movies"]["rows"][0]["checks"], [])

    def test_a_run_with_no_verify_json_is_every_library_not_run(self):
        doc = verify_report.build(str(self.run_dir), "x")
        self.assertEqual((doc["verdict"], doc["detail"]), ("nothing verified", {"exit": None, "libraries": {"movies": None, "tvshows": None}}))

    def test_the_page_shows_each_row_its_checks_and_a_library_that_did_not_run(self):
        d = self.write_run(RAW)
        page = Path(verify_report.write_report(str(d), verify_report.build(str(d), "ferrofin:v9"))).read_text()
        for text in ("<h1>Verify: failed</h1>", "The movies library", "1 PASS · 1 FAIL · 1 SKIP",
                     "<span class='chip pass'>PASS</span>", "<span class='chip fail'>FAIL</span>", "<span class='chip skip'>SKIP</span>",
                     "rescan, nothing changed", "<td>updated</td><td class='num bad'>1</td><td class='num'>= 0</td><td>failed</td>",
                     "<td class='num bad'>&quot;old &lt;b&gt;&quot;</td><td class='num'>= &quot;new&quot;</td>",
                     "<td>the edit was refused</td><td class='num bad'>—</td><td class='num'></td>",
                     "The tvshows library</h2><p class='problem'>Did not run: the check ended before this library's rows (the run's exit 1)",
                     "<a href='server.log'>server log</a>"):
            self.assertIn(text, page)
        failed = page[page.index("rescan, nothing changed"):page.index("a webhook scan")]
        self.assertIn("<details open>", failed)
        self.assertIn("<a href='server.log'>server log</a>", failed)
        passed = page[page.index("first scan of new items"):page.index("rescan, nothing changed")]
        self.assertIn("<details>", passed)
        self.assertNotIn("server log", passed)
        self.assertNotRegex(page, r"(src|href)=['\"]https?://")   # self-contained: opens from disk
        self.assertIn("<style>", page)

    def test_without_a_server_log_nothing_links_one(self):
        d = self.write_run(RAW, log=False)
        page = Path(verify_report.write_report(str(d), verify_report.build(str(d), "x"))).read_text()
        self.assertNotIn("server.log", page)


class ResultEnvelopeTests(unittest.TestCase):
    def test_problems_name_what_is_wrong(self):
        result = verify_report.result
        good = result.envelope("verify", "passed", {"pass": 1}, {"image": "x"}, None, "2026-10-25T12:00:00+00:00", {})
        self.assertEqual(result.problems(good), [])
        for change, problem in (({"schema": 2}, "schema is 2, not 1"), ({"gate": "other"}, "gate 'other' is not one of"),
                                ({"verdict": ""}, "verdict is not a non-empty string"),
                                ({"counts": {"pass": "1"}}, "counts is not an object of whole numbers"),
                                ({"counts": {"pass": True}}, "counts is not an object of whole numbers"),
                                ({"source": {"image": "x"}}, "source does not hold exactly"),
                                ({"source": {**good["source"], "commit": 5}}, "source.commit is neither null nor a string"),
                                ({"source": {**good["source"], "host_class": "big"}}, "source.host_class is neither null nor a object"),
                                ({"ended": 5}, "ended is neither null nor a timestamp string"),
                                ({"detail": []}, "detail is not an object"), ({"extra": 1}, "unknown extra")):
            with self.subTest(change):
                self.assertTrue(any(problem in p for p in result.problems({**good, **change})), result.problems({**good, **change}))
        self.assertEqual(result.problems([]), ["the result is not a JSON object"])
        self.assertIn("missing detail", result.problems({k: v for k, v in good.items() if k != "detail"})[0])
        with self.assertRaisesRegex(ValueError, "not a valid result.json"):
            result.write(tempfile.gettempdir(), {**good, "schema": 2})


if __name__ == "__main__":
    unittest.main()
