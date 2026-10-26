"""The memory soak: slope of anon memory over the window, the D7 verdict, the phase's
files, the A/B session report and run.sh's arguments (stdlib only; no server, no k6)."""
import csv
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import gate
import report
import soak

MIB = 2 ** 20
SOAK_PY = str(Path(__file__).parent / "soak.py")
GATE_PY = str(Path(__file__).parent / "gate.py")
RUN_SH = str(Path(__file__).parent / "run.sh")


def line(per_hour, start_mib=100.0, seconds=3600, step=1.0, t0=1000.0):
    """`(t, anon bytes)` samples of a series growing `per_hour` MiB an hour."""
    n = int(seconds / step) + 1
    return [(t0 + i * step, int((start_mib + per_hour * i * step / 3600) * MIB)) for i in range(n)]


class SlopeTests(unittest.TestCase):
    def test_slope_of_a_straight_line_is_its_rate_in_mib_per_hour(self):
        for rate in (0, 10, 20, -5):
            with self.subTest(rate):
                self.assertAlmostEqual(soak.slope(line(rate)), rate, places=3)

    def test_slope_uses_every_sample_not_just_the_ends(self):
        # A spike at the very end moves a two-point slope a lot and a least-squares fit little.
        samples = line(0)
        samples[-1] = (samples[-1][0], samples[-1][1] + 50 * MIB)
        self.assertLess(soak.slope(samples), 1.0)

    def test_slope_needs_two_distinct_times(self):
        self.assertIsNone(soak.slope([]))
        self.assertIsNone(soak.slope([(1.0, MIB)]))
        self.assertIsNone(soak.slope([(1.0, MIB), (1.0, 2 * MIB)]))

    def test_the_leading_warm_up_is_left_out_of_the_fit(self):
        # Heap growth in the first ten minutes, then flat: a leak-free server.
        samples = [(t, int((100 + min(t - 1000, 600) / 600 * 60) * MIB)) for t, _ in line(0)]
        window = {"start": 1000.0, "end": 4600.0}
        whole = soak.fit(samples, window, 0)
        skipped = soak.fit(samples, window, 600)
        self.assertGreater(whole["slope_mib_per_hour"], 20)
        self.assertAlmostEqual(skipped["slope_mib_per_hour"], 0, places=3)
        self.assertEqual(skipped["fit_from"], 1600.0)

    def test_only_samples_inside_the_window_count(self):
        samples = line(0, seconds=7200)
        window = {"start": 2000.0, "end": 4000.0}
        result = soak.fit(samples, window, 0)
        self.assertEqual((result["fit_from"], result["fit_to"]), (2000.0, 4000.0))
        self.assertEqual(result["samples"], 2001)

    def test_a_fit_with_too_few_samples_has_no_slope_and_says_why(self):
        result = soak.fit(line(5, seconds=1), {"start": 1000.0, "end": 1001.0}, 600)
        self.assertIsNone(result["slope_mib_per_hour"])
        self.assertIn("samples", result["reason"])


class VerdictTests(unittest.TestCase):
    LIMIT = 20

    def test_d7_regressed_only_when_the_candidate_is_over_and_the_previous_is_not(self):
        for name, prev, cand, expected in [
                ("regressed", 3, 25, "regressed"),
                ("limit itself is within", 3, 20, "within noise"),
                ("both under", 3, 12, "within noise"),
                ("both over", 30, 45, "within noise"),   # the previous already leaked: not a new problem
                ("improved", 30, 4, "improved"),
                ("negative slopes", -2, -3, "within noise")]:
            with self.subTest(name):
                self.assertEqual(soak.verdict(prev, cand, self.LIMIT), expected)

    def test_a_missing_slope_cannot_be_judged(self):
        self.assertEqual(soak.verdict(None, 3, self.LIMIT), "not comparable")
        self.assertEqual(soak.verdict(3, None, self.LIMIT), "not comparable")


class PhaseFileTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def run_dir(self, per_hour, name="run", skip_s=600, soak_s=3600):
        d = self.root / name / "ferrofin"
        d.mkdir(parents=True)
        samples = line(per_hour, seconds=soak_s + 120, step=1.0)
        with open(d / "mem.csv", "w", newline="") as f:
            w = csv.writer(f)
            w.writerow(["t", "anon", "file"])
            for t, anon in samples:
                w.writerow([f"{t:.3f}", anon, 0])
        (d / "windows.json").write_text(json.dumps({"soak": {"start": 1030.0, "end": 1030.0 + soak_s}, "steady": {"start": 5000.0, "end": 5060.0}}))
        (d / "soak-params.json").write_text(json.dumps({"rate": 5, "soak_s": soak_s, "skip_s": skip_s, "warmup_s": 30}))
        return d

    def test_finish_writes_the_slope_with_the_run_parameters(self):
        d = self.run_dir(12)
        doc = soak.finish(str(d))
        self.assertAlmostEqual(doc["slope_mib_per_hour"], 12, places=3)
        self.assertEqual((doc["rate"], doc["soak_s"], doc["skip_s"]), (5, 3600, 600))
        self.assertEqual(doc["fit_from"], 1630.0)
        self.assertEqual(json.loads((d / "soak.json").read_text())["slope_mib_per_hour"], doc["slope_mib_per_hour"])

    def test_cli_finish_exits_one_for_a_slope_it_could_not_fit_and_two_for_missing_files(self):
        d = self.run_dir(5, soak_s=3600)
        (d / "windows.json").write_text(json.dumps({"soak": {"start": 9e9, "end": 9e9 + 3600}}))
        none = subprocess.run([sys.executable, SOAK_PY, "finish", str(d)], capture_output=True, text=True)
        self.assertEqual(none.returncode, 1, none.stderr)
        gone = subprocess.run([sys.executable, SOAK_PY, "finish", str(self.root / "nowhere")], capture_output=True, text=True)
        self.assertEqual(gone.returncode, soak.CRASH)
        self.assertNotIn("Traceback", gone.stderr)

    def test_the_run_report_names_the_slope_and_its_fit_window(self):
        d = self.run_dir(12)
        soak.finish(str(d))
        for as_html in (False, True):
            page = report.soak_table({"runs": [str(d.parent)]}, as_html)
            self.assertIn("12.0 MiB/h", page)
            self.assertIn("600", page)
        self.assertEqual(report.soak_table({"runs": [str(self.root / "none")]}), "")


class SessionTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def session(self, slopes):
        self.count = getattr(self, "count", 0) + 1
        out = self.root / f"gate-soak-{self.count}"
        plan = {"mode": "soak", "name": out.name, "date": "2026-10-20T00:00:00+00:00",
                "schedule": gate.schedule("soak", {"a": "old", "b": "new"}, len(slopes) // 2),
                "images": {"a": {"ref": "old", "id": "sha256:old"}, "b": {"ref": "new", "id": "sha256:new"}}}
        out.mkdir()
        (out / "gate.json").write_text(json.dumps(plan))
        for (name, _, _), s in zip(plan["schedule"], slopes):
            d = out / name / "ferrofin"
            d.mkdir(parents=True)
            (d / "soak.json").write_text(json.dumps({"slope_mib_per_hour": s, "samples": 30000, "fit_from": 1.0, "fit_to": 2.0, "skip_s": 600, "soak_s": 3600, "rate": 5,
                                                     "peak_mib": 512.0, "reason": "too few samples"}))
        return out

    def test_the_gate_row_is_the_d7_limit(self):
        self.assertEqual(report.load_gate()["soak"], {"max_slope_mib_per_hour": 20})

    def test_schedule_is_one_back_to_back_pair_by_default_and_dry_run_starts_nothing(self):
        out = self.root / "session"
        result = subprocess.run([sys.executable, GATE_PY, "soak", "old", "new", "--dry-run", "--out", str(out)],
                                capture_output=True, text=True, env=dict(os.environ, PATH=""))  # no docker reachable
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.count("--only soak"), 2)
        self.assertIn("02-b  b  new", result.stdout)
        self.assertFalse(out.exists())

    def test_render_blocks_on_a_new_leak_only(self):
        for name, slopes, block in [("new leak", [2.0, 31.0], ["soak/slope"]),
                                    ("self A/B", [2.0, 2.5], []),
                                    ("leak fixed", [31.0, 2.0], [])]:
            with self.subTest(name):
                out = self.session(slopes)
                md, got = gate.render(str(out))
                self.assertEqual(got, block, md)
                self.assertEqual(json.loads((out / "verdict.json").read_text())["blocking"], block)
                self.assertIn("MiB/h", (out / "report.html").read_text())

    def test_the_soak_verdict_reaches_the_candidate_runs_in_the_viewer(self):
        out = self.session([2.0, 31.0])
        gate.render(str(out))
        cells = report.load_verdicts([str(out / "02-b")])
        self.assertEqual([(c["cell"], c["verdict"], c["baseline"], c["candidate"]) for c in cells],
                         [("soak/slope", "regressed", 2.0, 31.0)])
        self.assertIn("A/B gate — 1 regressed", (out / "02-b" / "report.html").read_text())
        self.assertIsNone(report.load_verdicts([str(out / "01-a")]))  # the previous release is not judged

    def test_the_soak_verdict_and_report_carry_the_oracle_and_a_newer_release(self):
        pin = {"tag": "v12.2", "image": "jf@sha256:abc",
               "note": "measured against Jellyfin v12.2, the release this branch pins; v12.3 is out"}
        out = self.session([2.0, 2.5])
        plan = json.loads((out / "gate.json").read_text())
        (out / "gate.json").write_text(json.dumps(plan | {"oracle": pin}))
        md, _ = gate.render(str(out))
        self.assertEqual(json.loads((out / "verdict.json").read_text())["oracle"], pin)
        self.assertIn(f"Jellyfin oracle v12.2 (jf@sha256:abc) — {pin['note']}", md)

    def test_an_unmeasured_soak_blocks_because_it_cannot_be_judged(self):
        out = self.session([2.0, None])
        md, got = gate.render(str(out))
        self.assertEqual(got, ["soak/slope"])
        self.assertIn("not comparable", md)


class RunShArgumentTests(unittest.TestCase):
    """run.sh's soak arguments, checked before any fixture or docker access."""

    def run_sh(self, *args, **env):
        return subprocess.run(["bash", RUN_SH, "--testdata", "/nonexistent-testdata", "--out", "/nonexistent-out", *args],
                              env=dict(os.environ, **env), capture_output=True, text=True)

    def test_the_soak_is_ferrofin_only(self):
        for servers in ([], ["--servers", "jellyfin"], ["--servers", "jellyfin,ferrofin"]):
            with self.subTest(servers):
                result = self.run_sh("--only", "soak", *servers)
                self.assertEqual(result.returncode, 1)
                self.assertIn("the soak is Ferrofin only", result.stderr)

    def test_the_soak_is_a_known_phase_and_reaches_the_fixture_check(self):
        for only in ("soak", "counts,soak"):
            with self.subTest(only):
                self.assertIn("build the test data", self.run_sh("--only", only, "--servers", "ferrofin").stderr)

    def test_the_fit_must_leave_part_of_the_window(self):
        refused = self.run_sh("--only", "soak", "--servers", "ferrofin", SOAK_S="600", SOAK_SKIP_S="600")
        self.assertIn("SOAK_SKIP_S must be shorter than SOAK_S", refused.stderr)
        # not selected: the soak's tunables are not this run's business
        other = self.run_sh("--only", "counts", "--servers", "ferrofin", SOAK_S="600", SOAK_SKIP_S="900")
        self.assertNotIn("SOAK_SKIP_S", other.stderr)
        self.assertIn("build the test data", other.stderr)

    def test_the_soak_tunables_must_be_positive_integers(self):
        for name in ("SOAK_S", "RATE_SOAK", "SOAK_SKIP_S"):
            with self.subTest(name):
                result = self.run_sh("--only", "counts", **{name: "0"})
                self.assertEqual(result.returncode, 2)
                self.assertIn(f"{name} must be a positive integer", result.stderr)


if __name__ == "__main__":
    unittest.main()
