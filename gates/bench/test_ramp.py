"""The saturation ramp: ceiling from a step series, the A/B verdict on ceilings, the
phase's files and the session report (stdlib only; no server, no k6)."""
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import gate
import ramp
import report

BOUND = 100
RAMP_PY = str(Path(__file__).parent / "ramp.py")
GATE_PY = str(Path(__file__).parent / "gate.py")


def step(rate, p95=20, ok=1.0, dropped=0, iterations=100):
    return {"rate": rate, "p95": p95, "ok": ok, "dropped": dropped, "iterations": iterations}


def summary(p95s, ok=1, dropped=0):
    """A screens.js summary with one screen per p95."""
    return {"iterations": 100, "dropped_iterations": dropped,
            "screens": {f"s{i}": {"p95": p, "ok": ok} for i, p in enumerate(p95s)}}


class CeilingTests(unittest.TestCase):
    def test_ceiling_is_the_last_passing_rate_before_the_first_failure(self):
        steps = [step(25), step(35), step(45, p95=99), step(55, p95=101), step(65, p95=20)]
        self.assertEqual(ramp.ceiling(steps, BOUND),
                         {"ceiling": 45, "limit": "p95", "reason": "worst screen p95 101 ms is over the 100 ms bound",
                          "stopped_at": 55})  # a later passing step after the knee does not count

    def test_the_bound_itself_passes(self):
        self.assertEqual(ramp.ceiling([step(25, p95=100), step(35, p95=100.1)], BOUND)["ceiling"], 25)

    def test_steps_are_taken_in_rate_order(self):
        self.assertEqual(ramp.ceiling([step(45, p95=500), step(25), step(35)], BOUND)["ceiling"], 35)

    def test_each_way_a_step_can_end_the_ramp(self):
        for name, bad, limit in [("errors", step(35, ok=0.99), "errors"), ("client", step(35, dropped=3), "client"),
                                 ("no result", step(35, iterations=0), "invalid"), ("no screens", step(35, p95=None, ok=None), "invalid")]:
            with self.subTest(name):
                r = ramp.ceiling([step(25), bad], BOUND)
                self.assertEqual((r["ceiling"], r["limit"], r["stopped_at"]), (25, limit, 35))

    def test_errors_outrank_a_slow_p95_and_a_dropped_client(self):
        self.assertEqual(ramp.ceiling([step(25), step(35, p95=500, ok=0.5, dropped=9)], BOUND)["limit"], "errors")

    def test_no_failure_means_the_cap_is_a_lower_bound(self):
        r = ramp.ceiling([step(25), step(35)], BOUND)
        self.assertEqual((r["ceiling"], r["limit"], r["stopped_at"]), (35, "cap", None))

    def test_first_step_failing_has_no_ceiling(self):
        r = ramp.ceiling([step(25, p95=300)], BOUND)
        self.assertEqual((r["ceiling"], r["limit"]), (None, "p95"))
        self.assertFalse(ramp.comparable(r)[0])

    def test_only_server_limits_and_the_cap_are_comparable(self):
        for limit, expected in [("p95", True), ("errors", True), ("cap", True), ("client", False), ("invalid", False)]:
            with self.subTest(limit):
                self.assertEqual(ramp.comparable({"ceiling": 45, "limit": limit, "reason": "r"})[0], expected)

    def test_step_record_takes_the_worst_screen(self):
        self.assertEqual(ramp.step_record(25, summary([20, 80, 40], ok=0.998)),
                         {"rate": 25, "p95": 80, "ok": 0.998, "dropped": 0, "iterations": 100})
        self.assertIsNone(ramp.step_record(25, {})["p95"])


class VerdictTests(unittest.TestCase):
    BAND = {"relative_pct": 15, "absolute": 5}

    def test_a_falling_ceiling_regresses_and_a_rising_one_improves(self):
        for name, a, b, expected in [
                ("regressed", [200, 200, 200], [160, 170, 160], "regressed"),
                ("improved", [160, 170, 160], [200, 200, 200], "improved"),
                ("equal", [200, 200, 200], [200, 200, 200], "within noise"),
                ("relative only", [20, 20, 20], [17, 17, 17], "within noise"),   # -15% is not past 15%, and 3 < 5
                ("absolute only", [400, 400, 400], [390, 390, 390], "within noise"),
                ("one pair worse", [200, 200, 200], [100, 205, 205], "within noise")]:
            with self.subTest(name):
                self.assertEqual(gate.verdict(a, b, self.BAND, higher_is_better=True)[0], expected)

    def test_lower_is_still_better_by_default(self):
        self.assertEqual(gate.verdict([100, 100, 100], [130, 125, 130], self.BAND)[0], "regressed")

    def docs(self, ceilings, **kw):
        return [{"ceiling": c, "limit": "p95", "reason": "r", **kw} for c in ceilings]

    def test_ramp_verdicts_block_on_a_regression_or_an_unjudgeable_ramp(self):
        g = report.load_gate()
        for name, a, b, verdict, blocks in [
                ("self A/B", self.docs([185] * 3), self.docs([185] * 3), "within noise", False),
                ("regressed", self.docs([185] * 3), self.docs([135] * 3), "regressed", True),
                ("improved", self.docs([135] * 3), self.docs([185] * 3), "improved", False),
                ("client bound", self.docs([185] * 3), self.docs([185] * 3, limit="client"), "not comparable", True),
                ("no ramp", self.docs([185] * 3), [None] * 3, "not comparable", True)]:
            with self.subTest(name):
                rows = gate.ramp_verdicts(a, b, g)
                self.assertEqual(rows["ramp/ceiling"]["verdict"], verdict)
                self.assertEqual(bool(gate.blocking(rows)), blocks)

    def test_a_per_cell_band_overrides_the_row(self):
        g = {**report.load_gate(), "cells": {"ramp/ceiling": {"relative_pct": 50, "absolute": 5}}}
        self.assertEqual(gate.ramp_verdicts(self.docs([200] * 3), self.docs([150] * 3), g)["ramp/ceiling"]["verdict"], "within noise")


class PhaseFileTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def write_run(self, run, p95_by_rate):
        d = run / "ferrofin"
        d.mkdir(parents=True)
        (d / "ramp-params.json").write_text(json.dumps({"start": 25, "step": 10, "step_s": 30, "cap": 400, "bound_ms": BOUND, "warmup_s": 30}))
        (d / "k6-ramp-warmup.json").write_text(json.dumps(summary([5])))  # not a step
        for rate, p95 in p95_by_rate.items():
            (d / f"k6-ramp-{rate}.json").write_text(json.dumps(summary([p95, p95 / 2])))
        return d

    def test_finish_writes_the_ceiling_from_the_step_files(self):
        d = self.write_run(self.root / "run", {25: 20, 35: 30, 45: 90, 55: 150})
        doc = ramp.finish(str(d))
        self.assertEqual((doc["ceiling"], doc["limit"], doc["stopped_at"]), (45, "p95", 55))
        self.assertEqual([s["rate"] for s in doc["steps"]], [25, 35, 45, 55])
        self.assertEqual(json.loads((d / "ramp.json").read_text())["ceiling"], 45)
        self.assertEqual(ramp.rows(doc)[-1][-1], "stopped: worst screen p95 150 ms is over the 100 ms bound")

    def test_check_exits_nonzero_with_the_reason_for_a_failed_step(self):
        d = self.write_run(self.root / "run", {25: 20, 35: 150})
        ok = subprocess.run([sys.executable, RAMP_PY, "check", str(d / "k6-ramp-25.json"), "100"], capture_output=True, text=True)
        bad = subprocess.run([sys.executable, RAMP_PY, "check", str(d / "k6-ramp-35.json"), "100"], capture_output=True, text=True)
        self.assertEqual((ok.returncode, ok.stdout), (0, ""))
        self.assertEqual(bad.returncode, 1)
        self.assertIn("over the 100 ms bound", bad.stdout)

    def test_finish_with_no_passing_step_exits_one(self):
        d = self.write_run(self.root / "run", {25: 500})
        result = subprocess.run([sys.executable, RAMP_PY, "finish", str(d)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 1)

    def test_unreadable_input_exits_two_so_it_is_not_taken_for_a_failed_step(self):
        d = self.write_run(self.root / "run", {25: 20})
        (d / "k6-ramp-25.json").write_text("{not json")
        for args in (["check", str(d / "k6-ramp-25.json"), "100"], ["check", str(d / "missing.json"), "100"],
                     ["finish", str(d)], ["finish", str(self.root / "nowhere")]):
            with self.subTest(args):
                result = subprocess.run([sys.executable, RAMP_PY, *args], capture_output=True, text=True)
                self.assertEqual(result.returncode, ramp.CRASH)
                self.assertNotIn("Traceback", result.stderr)


class RunShArgumentTests(unittest.TestCase):
    """run.sh's ramp arguments, checked before any fixture or docker access."""

    def run_sh(self, *args, **env):
        return subprocess.run(["bash", str(Path(__file__).parent / "run.sh"), "--testdata", "/nonexistent-testdata", "--out", "/nonexistent-out", *args],
                              env=dict(os.environ, **env), capture_output=True, text=True)

    def test_the_ramp_is_ferrofin_only(self):
        for servers in ([], ["--servers", "jellyfin"], ["--servers", "jellyfin,ferrofin"]):
            with self.subTest(servers):
                result = self.run_sh("--only", "ramp", *servers)
                self.assertEqual(result.returncode, 1)
                self.assertIn("the ramp is Ferrofin only", result.stderr)

    def test_the_ramp_is_a_known_phase_and_reaches_the_fixture_check(self):
        for only in ("ramp", "counts,ramp"):
            with self.subTest(only):
                result = self.run_sh("--only", only, "--servers", "ferrofin")
                self.assertIn("build the test data", result.stderr)

    def test_a_start_above_the_cap_is_refused_only_when_the_ramp_is_selected(self):
        refused = self.run_sh("--only", "ramp", "--servers", "ferrofin", RAMP_START="500")
        self.assertIn("RAMP_START must not exceed RAMP_CAP", refused.stderr)
        # RATE_STRESS (the default start) above the cap is a documented knob for every other phase
        other = self.run_sh("--only", "counts", "--servers", "ferrofin", RATE_STRESS="500")
        self.assertNotIn("RAMP_START", other.stderr)
        self.assertIn("build the test data", other.stderr)

    def test_the_ramp_tunables_must_be_positive_integers(self):
        for name in ("RAMP_START", "RAMP_STEP_RATE", "RAMP_STEP_S", "RAMP_CAP", "RAMP_BOUND_MS"):
            with self.subTest(name):
                result = self.run_sh("--only", "counts", **{name: "0"})
                self.assertEqual(result.returncode, 2)
                self.assertIn(f"{name} must be a positive integer", result.stderr)


class SessionTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def session(self, ceilings):
        out = self.root / "gate-ramp"
        plan = {"mode": "ramp", "name": out.name, "date": "2026-10-20T00:00:00+00:00",
                "schedule": gate.schedule("ramp", {"a": "old", "b": "new"}, len(ceilings) // 2),
                "images": {"a": {"ref": "old", "id": "sha256:old"}, "b": {"ref": "new", "id": "sha256:new"}}}
        out.mkdir()
        (out / "gate.json").write_text(json.dumps(plan))
        for (name, _, _), c in zip(plan["schedule"], ceilings):
            d = out / name / "ferrofin"
            d.mkdir(parents=True)
            (d / "ramp.json").write_text(json.dumps({"start": 25, "step": 10, "step_s": 30, "cap": 400, "bound_ms": BOUND, "warmup_s": 30,
                                                     "ceiling": c, "limit": "p95", "reason": "r", "stopped_at": c + 10, "steps": []}))
        return out

    def test_schedule_runs_only_the_ramp_and_dry_run_starts_nothing(self):
        out = self.root / "session"
        result = subprocess.run([sys.executable, GATE_PY, "ramp", "old", "new", "--dry-run", "--out", str(out)],
                                capture_output=True, text=True, env=dict(os.environ, PATH=""))  # no docker reachable
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.count("--only ramp"), 6)
        self.assertIn("06-b  b  new", result.stdout)
        self.assertFalse(out.exists())

    def test_render_reports_a_regression_and_blocks(self):
        out = self.session([185, 135, 185, 135, 185, 135])
        md, block = gate.render(str(out))
        self.assertEqual(block, ["ramp/ceiling"])
        self.assertIn("**BLOCKED**", md)
        self.assertIn("class='delta bad'>regressed", (out / "report.html").read_text())
        self.assertEqual(json.loads((out / "verdict.json").read_text())["blocking"], block)

    def test_the_ramp_verdict_reaches_the_candidate_runs_in_the_viewer(self):
        out = self.session([185, 135, 185, 135, 185, 135])
        gate.render(str(out))
        for name in ("02-b", "04-b", "06-b"):
            with self.subTest(name):
                cells = report.load_verdicts([str(out / name)])
                self.assertEqual([(c["cell"], c["verdict"], c["baseline"], c["candidate"]) for c in cells],
                                 [("ramp/ceiling", "regressed", 185, 135)])
                self.assertIn("A/B gate — 1 regressed", (out / name / "report.html").read_text())
        self.assertIsNone(report.load_verdicts([str(out / "01-a")]))  # the previous release is not judged

    def test_self_ab_reports_no_regression(self):
        md, block = gate.render(str(self.session([185] * 6)))
        self.assertEqual(block, [])
        self.assertIn("**PASS**", md)


class RunReportTests(unittest.TestCase):
    def test_the_run_report_shows_the_ceiling_and_the_step_that_ended_the_ramp(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = Path(tmp) / "run"
            d = run / "ferrofin"
            d.mkdir(parents=True)
            steps = [step(25), step(35, p95=130)]
            (d / "ramp.json").write_text(json.dumps({
                "start": 25, "step": 10, "step_s": 30, "cap": 400, "bound_ms": BOUND, "warmup_s": 30,
                **ramp.ceiling(steps, BOUND), "steps": steps}))
            for as_html in (False, True):
                page = report.ramp_table({"runs": [str(run)]}, as_html)
                self.assertIn("ceiling 25 screens/s", page)
                self.assertIn("stopped: worst screen p95 130 ms", page)
            self.assertEqual(report.ramp_table({"runs": [str(Path(tmp) / "none")]}), "")


if __name__ == "__main__":
    unittest.main()
