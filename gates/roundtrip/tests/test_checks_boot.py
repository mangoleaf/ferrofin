"""checks_boot.py against Jellyfin console logs in their real shape (jellyfin-ok.log and
jellyfin-start-failed.log: lines taken from Jellyfin 12.4's own logs, a clean upgrade boot and the failed start
on a 10.11.8 database Ferrofin adopted)."""

from contextlib import redirect_stderr, redirect_stdout
import io
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.append(str(Path(__file__).resolve().parent.parent))
import checks_boot  # noqa: E402

HERE = Path(__file__).resolve().parent
#: the canned logs roundtrip.bats's fake docker also prints
CLEAN = (HERE / "jellyfin-ok.log").read_text()
FAILED = (HERE / "jellyfin-start-failed.log").read_text()


class Migrations(unittest.TestCase):
    def test_every_begun_migration_finished(self):
        self.assertEqual(checks_boot.migrations(CLEAN), [])

    def test_a_failed_migration_is_named_with_its_sqlite_error(self):
        self.assertEqual(checks_boot.migrations(FAILED), [
            "migration 20260113102337_AddLinkedChildrenTable failed: "
            "SQLite Error 1: 'table \"LinkedChildren\" already exists'"])

    def test_without_a_sqlite_error_the_first_error_is_the_cause(self):
        log = ("[01:00:00] [INF] [1] InternalCodeMigration: Perform migration 2026_Repair\n"
               "[01:00:00] [ERR] [1] InternalCodeMigration: Migration 2026_Repair failed\n")
        self.assertEqual(checks_boot.migrations(log),
                         ["migration 2026_Repair failed: InternalCodeMigration: Migration 2026_Repair failed"])

    def test_a_migration_cut_short_with_no_error_still_fails(self):
        log = "[01:00:00] [INF] [1] InternalDatabaseMigration: Perform migration 2026_Cut\n"
        self.assertEqual(checks_boot.migrations(log), ["migration 2026_Cut failed: no error logged after it began"])


class Errors(unittest.TestCase):
    def test_the_blocked_plugin_repository_is_expected(self):
        self.assertEqual(checks_boot.errors(CLEAN), [])

    def test_unexpected_errors_are_counted_and_the_first_quoted(self):
        reasons = checks_boot.errors(FAILED)
        self.assertEqual(len(reasons), 1)
        self.assertTrue(reasons[0].startswith("4 unexpected error(s): [ERR] Microsoft.EntityFrameworkCore"), reasons[0])
        self.assertIn("[FTL] InternalDatabaseMigration: Error: SQLite Error 1", reasons[0])
        self.assertTrue(reasons[0].endswith("; and 1 more"), reasons[0])

    def test_a_continuation_line_is_never_an_entry(self):
        log = "[01:00:00] [INF] [1] Main: starting\n[ERR] looks like a level but continues the entry above\n"
        self.assertEqual(checks_boot.errors(log), [])


class CommandLine(unittest.TestCase):
    def run_on(self, mode, text):
        with tempfile.NamedTemporaryFile("w", suffix=".log") as log, redirect_stdout(io.StringIO()):
            log.write(text)
            log.flush()
            return checks_boot.main([mode, log.name])

    def test_exit_status(self):
        self.assertEqual(self.run_on("migrations", CLEAN), 0)
        self.assertEqual(self.run_on("migrations", FAILED), 1)
        self.assertEqual(self.run_on("errors", FAILED), 1)
        self.assertEqual(self.run_on("migrations", ""), 2)   # an empty capture never passes
        self.assertEqual(self.run_on("errors", "\x1b[32minfo\x1b[0m: not Jellyfin's format\n"), 2)
        with redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
            self.assertEqual(checks_boot.main(["errors", "/nonexistent/jellyfin.log"]), 2)
            self.assertEqual(checks_boot.main(["neither", "x"]), 2)


if __name__ == "__main__":
    unittest.main()
