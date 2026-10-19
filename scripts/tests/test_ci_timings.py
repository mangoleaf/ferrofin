"""Log parsing of scripts/ci-timings.py, on canned job logs:
python3 -m unittest discover -s scripts/tests -p 'test_*.py'."""
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("ci_timings", Path(__file__).parents[1] / "ci-timings.py")
ci_timings = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ci_timings)
ts, parse_log, window, cache_hit, read_test_step = (
    ci_timings.ts, ci_timings.parse_log, ci_timings.window, ci_timings.cache_hit, ci_timings.read_test_step)

ESC = "\x1b"

# Trimmed from a real lint job: rust-cache, clippy, the nextest step, then the doctest step.
NEXTEST_LOG = f"""\ufeff2026-10-19T05:44:06.6059500Z Current runner version: '2.337.0'
2026-10-19T05:44:18.5656216Z ##[group]Run Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6
2026-10-19T05:44:19.1464572Z No cache found.
2026-10-19T05:44:22.6761574Z ##[group]Run cargo clippy --all-targets --all-features -- -D warnings
2026-10-19T05:47:35.0339666Z {ESC}[1m{ESC}[92m    Finished{ESC}[0m `dev` profile [unoptimized] target(s) in 3m 12s
2026-10-19T05:47:35.2977391Z ##[group]Run FERROFIN_WASM_GUEST_TESTS=1 cargo nextest run --workspace
2026-10-19T05:52:07.6898899Z {ESC}[1m{ESC}[92m    Finished{ESC}[0m `test` profile [unoptimized] target(s) in 4m 32s
2026-10-19T06:01:24.2486806Z {ESC}[32;1m     Summary{ESC}[0m [ 554.614s] {ESC}[1m10380{ESC}[0m tests run: {ESC}[1m10380{ESC}[0m {ESC}[32;1mpassed{ESC}[0m, {ESC}[1m5{ESC}[0m {ESC}[33;1mskipped{ESC}[0m
2026-10-19T06:01:24.3170838Z ##[group]Run cargo test --workspace --doc
2026-10-19T06:01:25.3244688Z {ESC}[1m{ESC}[92m    Finished{ESC}[0m `test` profile [unoptimized] target(s) in 0.86s
2026-10-19T06:01:28.9517262Z test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
"""

# A `cargo test` step (no nextest) and a cache hit.
CARGO_TEST_LOG = """\ufeff2026-10-17T04:40:09.1171088Z ... Restoring cache ...
2026-10-17T04:40:09.2418801Z Cache hit for: v0-rust-lint-Linux-x64-050bb388-b4e2cf95
2026-10-17T04:40:27.4695463Z Cache restored successfully
2026-10-17T04:40:27.5048488Z Restored from cache key "v0-rust-lint-Linux-x64-050bb388-b4e2cf95" full match: true.
2026-10-17T04:43:00.1000000Z ##[group]Run FERROFIN_WASM_GUEST_TESTS=1 cargo test --workspace
2026-10-17T04:47:00.5000000Z     Finished `test` profile [unoptimized] target(s) in 4m 00s
2026-10-17T04:48:00.1000000Z test result: ok. 120 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out; finished in 1.20s
2026-10-17T04:57:59.9000000Z test result: FAILED. 30 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 9.00s
"""

# A coverage shard gating two crates: two builds, two nextest runs.
COVERAGE_LOG = """2026-10-19T05:44:20.7461757Z ##[group]Run scripts/coverage-gate.sh "$SHARD"
2026-10-19T05:46:34.2106090Z     Finished `test` profile [unoptimized] target(s) in 2m 09s
2026-10-19T05:50:44.1943506Z      Summary [ 249.683s] 3506 tests run: 3506 passed, 0 skipped
2026-10-19T05:51:00.0000000Z     Finished `test` profile [unoptimized] target(s) in 10s
2026-10-19T05:51:05.0000000Z      Summary [   4.000s] 1 test run: 1 passed, 0 skipped
"""


class Steps(unittest.TestCase):
    def test_matches_real_step_names_by_what_they_run(self):
        names = {
            "free runner disk (~25 GB of preinstalled toolchains)": "free disk",
            "Run Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6": "rust-cache restore",
            "Post Run Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6": None,
            "Run cargo clippy --all-targets --all-features -- -D warnings": "clippy",
            "Run FERROFIN_WASM_GUEST_TESTS=1 cargo nextest run --workspace": "tests",
            "Run FERROFIN_WASM_GUEST_TESTS=1 cargo test --workspace": "tests",
            "Run cargo test --workspace --doc": None,
            "coverage gate (per crate)": "tests",
            "Run shellcheck -x scripts/version.sh scripts/coverage-gate.sh": None,
        }
        for name, want in names.items():
            got = [label for label, pat in ci_timings.STEPS.items() if pat.search(name)]
            self.assertEqual(got, [want] if want else [], name)


class ParseLog(unittest.TestCase):
    def test_strips_the_bom_and_colour_and_parses_7_digit_fractions(self):
        lines = parse_log(NEXTEST_LOG)
        self.assertEqual(lines[0], (ts("2026-10-19T05:44:06.605950Z"), "Current runner version: '2.337.0'"))
        self.assertEqual(lines[4][1], "    Finished `dev` profile [unoptimized] target(s) in 3m 12s")

    def test_skips_lines_without_a_timestamp(self):
        self.assertEqual(parse_log("no stamp here\n\n"), [])


class Window(unittest.TestCase):
    def test_takes_whole_seconds_of_the_step(self):
        lines = parse_log(NEXTEST_LOG)
        # The API truncates: a line at 06:01:24.24 belongs to a step ending "06:01:24".
        got = window(lines, ts("2026-10-19T05:47:35Z"), ts("2026-10-19T06:01:24Z"))
        # From its own header to the next step's: clippy's last line (05:47:35.03) and the
        # doctest header (06:01:24.32) share its first and last seconds but are cut.
        self.assertEqual([line.split()[0] for _, line in got], ["##[group]Run", "Finished", "Summary"])

    def test_a_step_within_one_second_is_not_trimmed(self):
        got = window(parse_log(NEXTEST_LOG), ts("2026-10-19T05:44:18Z"), ts("2026-10-19T05:44:18Z"))
        self.assertEqual(len(got), 1)


class CacheHit(unittest.TestCase):
    def test_hit_miss_and_unknown(self):
        self.assertIs(cache_hit(parse_log(CARGO_TEST_LOG)), True)
        self.assertIs(cache_hit(parse_log(NEXTEST_LOG)), False)
        self.assertIsNone(cache_hit(parse_log(COVERAGE_LOG)))


class ReadTestStep(unittest.TestCase):
    def test_nextest_split_and_count(self):
        lines = parse_log(NEXTEST_LOG)
        compile_s, exec_s, count = read_test_step(lines, ts("2026-10-19T05:47:35Z"), ts("2026-10-19T06:01:24Z"))
        self.assertAlmostEqual(compile_s, 272.68989, places=4)  # 05:47:35 -> 05:52:07.689
        self.assertAlmostEqual(exec_s, 556.31011, places=4)  # 05:52:07.689 -> 06:01:24
        self.assertEqual(count, 10380)

    def test_doctest_step_is_its_own_window(self):
        # It starts in the second nextest's Summary ends: that Summary is not its count.
        _, _, count = read_test_step(parse_log(NEXTEST_LOG), ts("2026-10-19T06:01:24Z"), ts("2026-10-19T06:01:32Z"))
        self.assertEqual(count, 3)

    def test_next_steps_build_in_the_shared_last_second_keeps_the_split(self):
        # A warm doctest build finishing within the second the nextest step ended.
        log = NEXTEST_LOG.replace("2026-10-19T06:01:25.3244688Z", "2026-10-19T06:01:24.9244688Z")
        compile_s, _, count = read_test_step(parse_log(log), ts("2026-10-19T05:47:35Z"), ts("2026-10-19T06:01:24Z"))
        self.assertAlmostEqual(compile_s, 272.68989, places=4)
        self.assertEqual(count, 10380)

    def test_cargo_test_counts_passed_and_failed_not_ignored(self):
        compile_s, exec_s, count = read_test_step(
            parse_log(CARGO_TEST_LOG), ts("2026-10-17T04:43:00Z"), ts("2026-10-17T04:58:00Z"))
        self.assertAlmostEqual(compile_s, 240.5, places=4)
        self.assertAlmostEqual(exec_s, 659.5, places=4)
        self.assertEqual(count, 151)

    def test_several_builds_are_counted_but_not_split(self):
        compile_s, exec_s, count = read_test_step(
            parse_log(COVERAGE_LOG), ts("2026-10-19T05:44:20Z"), ts("2026-10-19T05:51:08Z"))
        self.assertEqual((compile_s, exec_s, count), (None, None, 3507))

    def test_no_test_output(self):
        self.assertEqual(read_test_step([], ts("2026-10-19T05:44:20Z"), ts("2026-10-19T05:51:08Z")), (None, None, None))


def step(name, start, end, conclusion="success"):
    return {"name": name, "started_at": start, "completed_at": end, "conclusion": conclusion}


# One lint run (the NEXTEST_LOG job), and a run whose every job was skipped by its `if:`.
RUNS = [{"id": 2, "created_at": "2026-10-19T05:44:03Z", "run_started_at": "2026-10-19T05:44:03Z"},
        {"id": 1, "created_at": "2026-10-19T05:00:00Z", "run_started_at": "2026-10-19T05:00:00Z"}]
JOBS = {
    2: [{"id": 20, "name": "lint", "conclusion": "success",
         "started_at": "2026-10-19T05:44:05Z", "completed_at": "2026-10-19T06:01:49Z", "steps": [
             step("free runner disk (~25 GB of preinstalled toolchains)", "2026-10-19T05:44:10Z",
                  "2026-10-19T05:44:10Z", "skipped"),
             step("Run Swatinem/rust-cache@6323deb", "2026-10-19T05:44:18Z", "2026-10-19T05:44:19Z"),
             step("Run cargo clippy --all-targets", "2026-10-19T05:44:22Z", "2026-10-19T05:47:35Z"),
             step("Run FERROFIN_WASM_GUEST_TESTS=1 cargo nextest run --workspace", "2026-10-19T05:47:35Z",
                  "2026-10-19T06:01:24Z"),
             step("Run cargo test --workspace --doc", "2026-10-19T06:01:24Z", "2026-10-19T06:01:32Z")]},
        {"id": 21, "name": "shell-tests", "conclusion": "success",
         "started_at": "2026-10-19T05:44:06Z", "completed_at": "2026-10-19T05:45:11Z", "steps": [
             step("Run shellcheck -x scripts/coverage-gate.sh", "2026-10-19T05:44:20Z", "2026-10-19T05:44:21Z")]},
        {"id": 22, "name": "benchmark-report", "conclusion": "skipped",
         "started_at": "2026-10-19T05:44:06Z", "completed_at": "2026-10-19T07:00:00Z", "steps": []}],
    1: [{"id": 10, "name": "lint", "conclusion": "skipped",
         "started_at": "2026-10-19T05:00:00Z", "completed_at": "2026-10-19T05:00:00Z", "steps": []}],
}


def fake_gh(*args):
    path = next(a for a in args if a.startswith("repos/"))
    if path.endswith("/logs"):
        assert path.endswith("/jobs/20/logs"), path  # shell-tests has no Rust step: no log fetched
        return NEXTEST_LOG
    return json.dumps({"jobs": JOBS[int(path.split("/")[-2])]})


class Collect(unittest.TestCase):
    @patch.object(ci_timings, "gh", side_effect=fake_gh)
    def test_reads_steps_by_name_and_skips_skipped_jobs_steps_and_runs(self, _):
        wall, samples = ci_timings.collect(RUNS)
        self.assertEqual(wall, [1066.0])  # run 1 had no successful job; benchmark-report was skipped
        self.assertEqual({k: [round(v, 2) for v in vs] for k, vs in samples["lint"].items()}, {
            "total": [1064.0], "rust-cache restore": [1.0], "cache hit": [0], "clippy": [193.0],
            "tests": [829.0], "test compile": [272.69], "test execution": [556.31],
            "tests run": [10380]})
        self.assertEqual(samples["shell-tests"], {"total": [65.0]})
        self.assertEqual(ci_timings.report(RUNS[:1], wall, samples).splitlines()[3:], [
            "run wall-clock                          1066 [1066-1066] (1)",
            "lint                                    1064 [1064-1064] (1)",
            "  rust-cache restore                    1 [1-1] (1)  cache hit 0/1",
            "  clippy                                193 [193-193] (1)",
            "  tests                                 829 [829-829] (1)",
            "  test compile                          273 [273-273] (1)",
            "  test execution                        556 [556-556] (1)",
            "  tests run                             10380 [10380-10380] (1)",
            "shell-tests                             65 [65-65] (1)"])


class SuccessfulRuns(unittest.TestCase):
    def created(self, after, before):
        with patch.object(ci_timings, "gh", return_value='{"workflow_runs": []}') as gh:
            ci_timings.successful_runs(5, after, before)
        args = gh.call_args.args
        self.assertIn("status=success", args)
        return next((a for a in args if a.startswith("created=")), None)

    def test_window_forms(self):
        self.assertEqual(self.created("2026-10-18", "2026-10-19"), "created=2026-10-18..2026-10-19")
        self.assertEqual(self.created("2026-10-18", None), "created=>=2026-10-18")
        self.assertEqual(self.created(None, "2026-10-19"), "created=<=2026-10-19")
        self.assertIsNone(self.created(None, None))

    def test_pages_until_n_or_a_short_page(self):
        pages = [json.dumps({"workflow_runs": [{"id": i} for i in range(100)]}),
                 json.dumps({"workflow_runs": [{"id": 100}]})]
        with patch.object(ci_timings, "gh", side_effect=pages) as gh:
            self.assertEqual(len(ci_timings.successful_runs(150, None, None)), 101)
        self.assertEqual(gh.call_count, 2)


if __name__ == "__main__":
    unittest.main()
