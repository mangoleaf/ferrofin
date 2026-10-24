#!/usr/bin/env python3
"""Tests of adoption/adoption_report.py: recording a run, the verdicts, the page and the shared viewer's listing.

    python3 -m unittest discover -s adoption/tests -p test_adoption_report.py
"""

import contextlib
import importlib.util
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import adoption_report as report  # noqa: E402

SHAPES = [{"shape": "deleted_user", "verdict": "PASS", "got": {"UserData": 0}, "want": {"UserData": 0}, "source": "Jellyfin 12.2"},
          {"shape": "guid_casing", "verdict": "FAIL", "got": {"owner": "lower"}, "want": {"owner": "upper"},
           "source": "written divergence: 0045 re-spells it"}]
SCALE = {"migration_seconds": 12.5, "memory_bytes": 3 * 1024 ** 3, "anon_bytes": None, "disk_bytes": 5 * 1024 ** 3,
         "wal_bytes": 1024 ** 2, "foreign_key_check_seconds": 0.4, "missing": ["anonymous memory: never sampled"]}


class Scratch(unittest.TestCase):
    def setUp(self):
        self._dir = tempfile.TemporaryDirectory()
        self.addCleanup(self._dir.cleanup)
        self.work = Path(self._dir.name) / "work"
        self.recorder = report.Recorder(self.work)

    def page(self):
        return self.recorder.build().read_text()


class Recording(Scratch):
    def test_events_are_appended_as_json_lines_and_start_forgets_the_previous_run(self):
        self.recorder.start("adoption/run.sh", image="ferrofin:x", oracle="12.2")
        self.recorder.check("fx", "after adoption", "boot log", "PASS")
        self.recorder.row("fx", "path", "12.2.0", "PASS")
        events = [json.loads(line) for line in (self.work / report.RESULTS).read_text().splitlines()]
        self.assertEqual([e["event"] for e in events], ["run", "check", "row"])
        self.recorder.build()
        self.assertTrue((self.work / report.REPORT).exists())
        self.recorder.start("again")
        self.assertEqual(len((self.work / report.RESULTS).read_text().splitlines()), 1)
        self.assertFalse((self.work / report.REPORT).exists())   # a stale page never outlives its run

    def test_a_verdict_or_kind_outside_the_vocabulary_is_refused(self):
        with self.assertRaisesRegex(ValueError, "verdict"):
            self.recorder.check("fx", "s", "c", "MAYBE")
        with self.assertRaisesRegex(ValueError, "a row is one of"):
            self.recorder.row("fx", "table", "12.2.0", "PASS")

    def test_a_run_killed_mid_write_leaves_a_report_of_what_was_recorded(self):
        self.recorder.start("run")
        self.recorder.check("fx", "after adoption", "boot log", "PASS")
        with open(self.work / report.RESULTS, "a") as output:
            output.write('{"event": "check", "fixt')   # the last line is cut off
        page = self.page()
        self.assertIn("boot log", page)
        self.assertIn("1 unreadable line(s)", page)


class Verdicts(Scratch):
    def run_rows(self, *rows):
        self.recorder.start("run")
        for fixture, check_verdicts, row_verdict in rows:
            for index, verdict in enumerate(check_verdicts):
                self.recorder.check(fixture, "after adoption", f"check {index}", verdict)
            if row_verdict:
                self.recorder.row(fixture, "path", "12.2.0", row_verdict)
        return report.load(self.work)[1]

    def test_a_row_is_its_own_verdict_unless_a_check_in_it_failed(self):
        rows = self.run_rows(("a", ["PASS"], "PASS"), ("b", ["PASS", "FAIL"], "PASS"), ("c", [], "SKIP"), ("d", ["PASS"], None))
        self.assertEqual([report.row_verdict(r) for r in rows], ["PASS", "FAIL", "SKIP", "INCOMPLETE"])

    def test_the_run_passes_only_when_a_fixture_passed_and_none_failed_or_stopped(self):
        self.assertEqual(report.overall(self.run_rows(("a", [], "PASS"), ("b", [], "SKIP"))), "PASS")
        self.assertEqual(report.overall(self.run_rows(("a", [], "PASS"), ("b", [], "FAIL"))), "FAIL")
        self.assertEqual(report.overall(self.run_rows(("a", [], "PASS"), ("b", ["PASS"], None))), "FAIL")
        self.assertEqual(report.overall(self.run_rows(("a", [], "SKIP"))), "NOTHING RAN")
        self.assertEqual(report.overall([]), "NOTHING RAN")


class Page(Scratch):
    def full_run(self):
        self.recorder.start("adoption/run.sh", image="ferrofin:adoption-ci", oracle="12.2")
        self.recorder.check("jellyfin-12.2-fresh", "after adoption", "boot log (generation, migrations, no ERROR)", "PASS")
        self.recorder.check("jellyfin-12.2-fresh", "after restart", "second boot runs no repair", "FAIL", "second boot repaired again: merged 3")
        self.recorder.row("jellyfin-12.2-fresh", "path", "12.2.0", "FAIL", "second boot repaired again: merged 3")
        self.recorder.check("jellyfin-12.2-messy", "after adoption", "table digests", "FAIL", "after adoption: tables: 2 difference(s)")
        self.recorder.attach("jellyfin-12.2-messy", "tables:after adoption", "Peoples row abc lost\nChapters row def changed")
        self.recorder.attach("jellyfin-12.2-messy", "shapes", SHAPES)
        self.recorder.row("jellyfin-12.2-messy", "messy", "12.2.0", "FAIL", "tables")
        self.recorder.attach("jellyfin-10.11.11-scale", "scale", SCALE)
        self.recorder.row("jellyfin-10.11.11-scale", "scale", "10.11.11", "PASS", "migration 12.5 s")
        self.recorder.row("jellyfin-12.0", "path", "12.0.0", "SKIP", "fixture missing")
        return self.page()

    def test_the_page_names_the_run_the_verdict_and_every_fixture(self):
        page = self.full_run()
        self.assertIn("adoption/run.sh", page)
        self.assertIn("ferrofin:adoption-ci", page)
        self.assertIn(">FAIL<", page)
        for fixture in ("jellyfin-12.2-fresh", "jellyfin-12.2-messy", "jellyfin-10.11.11-scale", "jellyfin-12.0"):
            self.assertIn(fixture, page)
        self.assertIn("fixture missing", page)

    def test_every_check_shows_its_stage_verdict_and_reason(self):
        page = self.full_run()
        self.assertIn("after restart", page)
        self.assertIn("second boot runs no repair", page)
        self.assertIn("second boot repaired again: merged 3", page)
        self.assertIn("boot log (generation, migrations, no ERROR)", page)

    def test_the_table_digest_differences_are_listed_by_stage(self):
        page = self.full_run()
        self.assertIn("Table-digest differences, after adoption", page)
        self.assertIn("Peoples row abc lost", page)

    def test_the_messy_shapes_show_both_end_states_and_where_the_expectation_comes_from(self):
        page = self.full_run()
        self.assertIn("Messy shapes: Ferrofin's end state against Jellyfin 12.2", page)
        self.assertIn("deleted_user", page)
        self.assertIn("written divergence: 0045 re-spells it", page)
        self.assertIn("&quot;owner&quot;: &quot;lower&quot;", page)
        self.assertIn("&quot;owner&quot;: &quot;upper&quot;", page)

    def test_the_scale_measurements_are_shown_with_units_and_what_was_not_measured(self):
        page = self.full_run()
        self.assertIn("12.5 s", page)
        self.assertIn("3.0 GiB", page)
        self.assertIn("not measured", page)
        self.assertIn("anonymous memory: never sampled", page)

    def test_failed_fixtures_are_open_and_passing_ones_are_collapsed(self):
        page = self.full_run()
        opened = [part for part in page.split("<details")[1:] if part.startswith(" open")]
        self.assertEqual(len(opened), 2)
        self.assertTrue(all("FAIL" in part.split("</summary>")[0] for part in opened))

    def test_text_from_the_run_cannot_inject_markup(self):
        self.recorder.start("<script>alert(1)</script>")
        self.recorder.check("<img src=x onerror=y>", "s", "c", "FAIL", "<b>boom</b>")
        self.recorder.row("<img src=x onerror=y>", "path", "1", "FAIL", "<b>boom</b>")
        page = self.page()
        for markup in ("<script>alert", "<img src=x", "<b>boom"):
            self.assertNotIn(markup, page)

    def test_the_page_is_self_contained(self):
        page = self.full_run()
        for external in ("src=", "href=", "http://", "https://", "@import"):
            self.assertNotIn(external, page)

    def test_an_empty_run_says_nothing_ran(self):
        self.recorder.start("adoption/run.sh")
        page = self.page()
        self.assertIn("NOTHING RAN", page)
        self.assertIn("No fixture was recorded.", page)

    def test_a_fixture_that_never_finished_is_incomplete_and_fails_the_run(self):
        self.recorder.start("run")
        self.recorder.check("fx", "after adoption", "boot log", "PASS")
        page = self.page()
        self.assertIn("INCOMPLETE", page)
        self.assertIn(">FAIL<", page)

    def test_a_fixture_that_began_and_was_interrupted_is_listed_unfinished_with_its_kind(self):
        self.recorder.start("run")
        self.recorder.begin("jellyfin-12.2-messy", "messy", "12.2.0")
        run, rows, _ = report.load(self.work)
        self.assertEqual([(r["fixture"], r["kind"], r["expected"], report.row_verdict(r)) for r in rows],
                         [("jellyfin-12.2-messy", "messy", "12.2.0", "INCOMPLETE")])
        self.assertIn("INCOMPLETE", self.page())
        self.recorder.row("jellyfin-12.2-messy", "messy", "12.2.0", "PASS")   # its own row settles it
        self.assertEqual(report.row_verdict(report.load(self.work)[1][0]), "PASS")


class CommandLine(Scratch):
    def run_cli(self, *argv):
        with contextlib.redirect_stdout(io.StringIO()) as out, contextlib.redirect_stderr(io.StringIO()) as err:
            code = report.main([str(a) for a in argv])
        return code, out.getvalue(), err.getvalue()

    def test_a_run_recorded_through_the_command_line_builds_the_same_page(self):
        shapes = Path(self._dir.name) / "shapes.json"
        shapes.write_text(json.dumps(SHAPES))
        text = Path(self._dir.name) / "diff.txt"
        text.write_text("Peoples row abc lost")
        steps = [("start", self.work, "--title", "adoption/run.sh", "--image", "img", "--oracle", "12.2"),
                 ("begin", self.work, "fx", "messy", "12.2.0"),
                 ("check", self.work, "fx", "after adoption", "ready", "PASS"),
                 ("check", self.work, "fx", "after adoption", "table digests", "FAIL", "--detail=tables: 1 difference(s)"),
                 ("attach", self.work, "fx", "shapes", shapes),
                 ("attach", self.work, "fx", "tables:after adoption", text, "--text"),
                 ("row", self.work, "fx", "messy", "12.2.0", "FAIL", "--detail=tables: 1 difference(s)")]
        for step in steps:
            self.assertEqual(self.run_cli(*step)[0], 0, step)
        code, out, _ = self.run_cli("build", self.work)
        page = (self.work / report.REPORT).read_text()
        self.assertEqual((code, out.strip()), (0, str(self.work / report.REPORT)))
        for text in ("table digests", "Peoples row abc lost", "guid_casing", ">FAIL<"):
            self.assertIn(text, page)

    def test_an_unusable_argument_fails_with_a_message_and_never_raises(self):
        code, _, err = self.run_cli("check", self.work, "fx", "s", "c", "MAYBE")
        self.assertEqual(code, 1)
        self.assertIn("verdict", err)
        code, _, err = self.run_cli("attach", self.work, "fx", "shapes", Path(self._dir.name) / "missing.json")
        self.assertEqual(code, 1)
        self.assertIn("report:", err)


class Robustness(Scratch):
    def run_cli(self, *argv, stdin=""):
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()) as err, \
                mock.patch.object(sys, "stdin", io.TextIOWrapper(io.BytesIO(stdin.encode("utf-8")))):
            code = report.main([str(a) for a in argv])
        return code, err.getvalue()

    def test_non_ascii_text_survives_a_c_locale_with_utf_8_mode_off(self):
        # the synthetic fixtures use non-ASCII names on purpose; the page is declared UTF-8, the digest logs are read as text
        import os
        import subprocess
        text = Path(self._dir.name) / "diff.txt"
        text.write_text("Peoples row 'Émile Zola' lost, Zoë Saldaña", encoding="utf-8")
        env = {**os.environ, "LC_ALL": "C", "LANG": "C", "PYTHONUTF8": "0", "PYTHONCOERCECLOCALE": "0", "PYTHONIOENCODING": "ascii"}
        script = str(Path(report.__file__))

        def cli(*argv, stdin=""):
            done = subprocess.run([sys.executable, script, *map(str, argv)], env=env, capture_output=True, input=stdin.encode("utf-8"))
            self.assertEqual(done.returncode, 0, done.stderr.decode("utf-8", "replace"))

        cli("start", self.work, "--title", "synthetic: Zoë")
        cli("check", self.work, "fx", "after adoption", "names", "FAIL", "--detail=STRAßE and İSTANBUL differ")
        cli("check", self.work, "fx", "after adoption", "more names", "FAIL", "--detail-stdin", stdin="Zoë Saldaña via stdin")
        cli("attach", self.work, "fx", "tables:after adoption", text, "--text")
        cli("row", self.work, "fx", "path", "1", "FAIL")
        cli("build", self.work)
        page = (self.work / report.REPORT).read_bytes().decode("utf-8")
        for expected in ("synthetic: Zoë", "STRAßE and İSTANBUL differ", "Zoë Saldaña via stdin", "Émile Zola"):
            self.assertIn(expected, page)

    def test_the_module_needs_nothing_beyond_the_standard_library(self):
        source = Path(report.__file__).read_text()
        for line in source.splitlines():
            if line.startswith(("import ", "from ")):
                module = line.split()[1].split(".")[0]
                self.assertIn(module, sys.stdlib_module_names, line)
        self.assertEqual(report.format_bytes(3 * 1024 ** 3), "3.0 GiB")
        self.assertEqual((report.format_bytes(512), report.format_bytes(1536)), ("512 B", "1.5 KiB"))

    def test_the_table_differences_of_a_stage_are_capped_and_the_rest_counted(self):
        lines = [f"Peoples row {n} lost" for n in range(report.MAX_DIFF_LINES + 37)]
        self.recorder.start("run")
        self.recorder.attach("fx", "tables:after adoption", "\n".join(lines))
        self.recorder.attach("fx", "tables:after restart", "\n".join(lines[:3]))   # a short list is kept whole
        self.recorder.row("fx", "path", "1", "FAIL")
        (row,) = report.load(self.work)[1]
        kept = row["attachments"]["tables:after adoption"].splitlines()
        self.assertEqual(len(kept), report.MAX_DIFF_LINES + 1)
        self.assertEqual(kept[-2], f"Peoples row {report.MAX_DIFF_LINES - 1} lost")
        self.assertIn("and 37 more", kept[-1])
        self.assertEqual(row["attachments"]["tables:after restart"].count("\n"), 2)

    def test_a_detail_that_starts_with_a_dash_or_is_huge_survives_through_the_command_line(self):
        huge = "x" * 300_000   # more than one argument can carry
        self.assertEqual(self.run_cli("check", self.work, "fx", "s", "c", "FAIL", "--detail=-5")[0], 0)
        self.assertEqual(self.run_cli("check", self.work, "fx", "s", "huge", "FAIL", "--detail-stdin", stdin=huge)[0], 0)
        self.assertEqual(self.run_cli("row", self.work, "fx", "path", "1", "FAIL", "--detail-stdin", stdin="-12")[0], 0)
        (row,) = report.load(self.work)[1]
        self.assertEqual([c["detail"] for c in row["checks"]], ["-5", huge])
        self.assertEqual(row["detail"], "-12")

    def test_an_io_error_while_recording_is_said_once_and_never_raised(self):
        blocker = Path(self._dir.name) / "a-file"
        blocker.write_text("not a directory")
        recorder = report.Recorder(blocker / "work")   # a path that can never be created
        with contextlib.redirect_stderr(io.StringIO()) as err:
            recorder.start("run")
            recorder.begin("fx", "path", "1")
            recorder.check("fx", "s", "c", "PASS")
            recorder.row("fx", "path", "1", "PASS")
            recorder.attach("fx", "shapes", [])
            self.assertIsNone(recorder.build())
        self.assertEqual(err.getvalue().count("cannot record"), 1)   # said once, not once per event
        self.assertIn("the report was not written", err.getvalue())

    def test_a_row_ends_the_fixtures_position_so_a_later_error_is_not_charged_to_it(self):
        self.recorder.start("run")
        self.recorder.where = ("a", "after scan")
        self.recorder.row("a", "matrix", "12.2", "PASS")
        self.assertIsNone(self.recorder.where)

    def test_the_reasons_of_a_check_or_a_row_are_capped_like_the_table_lists(self):
        reasons = [f"reason {n}" for n in range(report.MAX_DIFF_LINES + 5)]
        text = report.joined(reasons)
        self.assertEqual(text.count("; "), report.MAX_DIFF_LINES)   # 500 reasons, then the count
        self.assertTrue(text.endswith("… and 5 more"))
        self.assertEqual(report.joined(["a", "b"]), "a; b")
        self.assertEqual(report.joined([]), "")

    def test_a_hand_edited_or_corrupt_line_is_skipped_never_fatal(self):
        self.recorder.start("run")
        with open(self.work / report.RESULTS, "a") as output:
            for event in ({"event": "check", "fixture": "fx", "stage": "s", "check": "c", "verdict": 5},
                          {"event": "attach", "fixture": "fx", "data": "no key"},
                          {"event": "row", "fixture": "fx", "kind": "path", "expected": "1", "verdict": ["PASS"]},
                          {"event": "unknown", "fixture": "fx"},
                          {"event": [1], "fixture": "fx"},
                          {"event": "row", "fixture": "ok", "kind": "path", "expected": "1", "verdict": "PASS"}):
                output.write(json.dumps(event) + "\n")
        run, rows, skipped = report.load(self.work)
        self.assertEqual(([r["fixture"] for r in rows], skipped), (["ok"], 5))
        self.assertIn(">PASS<", self.page())

    def test_a_section_that_cannot_be_rendered_does_not_take_the_page_down(self):
        self.recorder.start("run")
        self.recorder.attach("fx", "shapes", [{"shape": "only-a-name"}])   # missing the keys the table needs
        self.recorder.check("fx", "after adoption", "boot log", "PASS")
        self.recorder.row("fx", "messy", "12.2.0", "PASS")
        page = self.page()
        self.assertIn("This section could not be rendered", page)
        self.assertIn("boot log", page)   # the rest of the page is there

    def test_start_marks_the_recorder_started_and_a_recorder_that_never_started_is_not(self):
        recorder = report.Recorder(self.work)
        self.assertFalse(recorder.started)
        recorder.start("run")
        self.assertTrue(recorder.started)


class SharedViewer(Scratch):
    """The report is a self-contained page in a work directory: bench/report.py --serve lists it."""

    @staticmethod
    def viewer():
        bench = Path(__file__).resolve().parents[2] / "bench"
        sys.path.insert(0, str(bench))   # the viewer imports its sibling modules
        try:
            spec = importlib.util.spec_from_file_location("bench_report", bench / "report.py")
            module = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(module)
        finally:
            sys.path.remove(str(bench))
        return module

    def test_the_adoption_tab_lists_a_work_directory_holding_a_report_and_the_directory_above_it(self):
        self.recorder.start("adoption/run.sh")
        self.recorder.row("fx", "path", "12.2.0", "PASS")
        self.recorder.build()
        viewer = self.viewer()
        # pointed at the work directory itself, it is the one run
        self.assertEqual([name for name, _ in viewer.discover("adoption", str(self.work))], ["work"])
        # pointed at the directory above (run.sh's WORK is DIR/work/run, synthetic.py's is DIR/work), it is listed by name
        self.assertEqual([name for name, _ in viewer.discover("adoption", str(self.work.parent))], ["work"])
        self.assertEqual(viewer.run_dir(str(self.work.parent), "work"), str(self.work.parent / "work"))


if __name__ == "__main__":
    unittest.main()
