"""checks_boot.py against Jellyfin console logs in their real shape (jellyfin-ok.log and
jellyfin-start-failed.log: lines taken from Jellyfin 12.4's own logs, a clean upgrade boot and the failed start
on a 10.11.8 database Ferrofin adopted)."""

from contextlib import closing, redirect_stderr, redirect_stdout
import io
import sqlite3
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.append(str(Path(__file__).resolve().parent.parent))
import checks_boot  # noqa: E402

HERE = Path(__file__).resolve().parent
#: the canned logs roundtrip.bats's fake docker prints, or derives its own from
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


def boot(*begun):
    """A log beginning (and finishing) each (source, migration) of `begun`."""
    return "[01:00:00] [INF] [1] Main: Jellyfin version: 12.4.0\n" + "".join(
        f"[01:00:00] [INF] [1] {source}: Perform migration {name}\n"
        f"[01:00:00] [INF] [1] {source}: Migration {name} was successfully applied\n" for source, name in begun)


CODE, SCHEMA = "InternalCodeMigration", "InternalDatabaseMigration"


class Ran(unittest.TestCase):
    #: Jellyfin's own first start on the source: a schema migration and two code migrations.
    ORACLE = boot((CODE, "2026_Fix"), (SCHEMA, "2026_AddTable"), (CODE, "2026_Repair"))

    def test_the_code_migrations_the_oracle_ran_and_no_schema_migration_pass(self):
        self.assertEqual(checks_boot.ran(boot((CODE, "2026_Repair"), (CODE, "2026_Fix")), self.ORACLE), [])
        self.assertEqual(checks_boot.ran(boot(), boot()), [])

    def test_a_history_recording_every_id_runs_none_of_them(self):
        self.assertEqual(checks_boot.ran(boot(), self.ORACLE),
                         ["2 code migration(s) it runs on the source did not run: 2026_Fix, 2026_Repair"])

    def test_a_schema_migration_or_an_unexpected_code_migration_is_named(self):
        log = boot((CODE, "2026_Fix"), (CODE, "2026_Repair"), (SCHEMA, "2026_AddTable"), (CODE, "2026_Other"))
        self.assertEqual(checks_boot.ran(log, self.ORACLE), [
            "it began 1 schema migration(s) the history should have recorded: 2026_AddTable",
            "1 code migration(s) ran that it does not run on the source: 2026_Other"])

    def test_a_code_migration_the_source_records_ran_there_only_after_a_revert(self):
        # Jellyfin 12.4 on 10.11.10: the first schema migration it applies reverts AddNormalizedUsername, whose Down
        # drops UpdateNormalizedUsername's row, so that routine runs again; on the export nothing reverts.
        oracle = boot((SCHEMA, "2026_AddLinkedChildrenTable"), (SCHEMA, "2026_AddNormalizedUsername"),
                      (CODE, "2026_UpdateNormalizedUsername"), (CODE, "2026_Repair"))
        source = {"2026_AddNormalizedUsername", "2026_UpdateNormalizedUsername"}
        self.assertEqual(checks_boot.ran(boot((CODE, "2026_Repair")), oracle, source), [])
        self.assertEqual(checks_boot.ran(boot(), oracle, source),
                         ["1 code migration(s) it runs on the source did not run: 2026_Repair"])

    def test_a_code_migration_the_export_records_as_applied_is_not_expected(self):
        # gl-1902: Jellyfin runs it on its own first start on the source, not on the export
        self.assertEqual(checks_boot.ran(boot((CODE, "2026_Fix")), self.ORACLE, applied={"2026_Repair"}), [])
        self.assertEqual(checks_boot.ran(boot((CODE, "2026_Fix"), (CODE, "2026_Repair")), self.ORACLE,
                                         applied={"2026_Repair"}),
                         ["1 code migration(s) ran that it does not run on the source: 2026_Repair"])

    def test_the_ids_recorded_as_applied_are_read_from_the_classification_table(self):
        with tempfile.TemporaryDirectory() as tmp:
            table = Path(tmp) / "classify.rs"
            table.write_text('    ("20200514181226_AddActivityLog", Schema),\n'
                             '    ("20260113120000_MigrateLinkedChildren", CodeRecordedAsApplied),\n'
                             '    ("20260113230000_CleanupOrphanedExtras", CodeLeftToRun),\n'
                             '/// ("20260115120000_Commented", CodeRecordedAsApplied),\n')
            self.assertEqual(checks_boot.recorded_as_applied(table), {"20260113120000_MigrateLinkedChildren"})
        # the checkout's own table: every id it records as applied, and nothing else
        text = checks_boot.CLASSIFY.read_text(encoding="utf-8")
        applied = checks_boot.recorded_as_applied()
        self.assertTrue(applied, "no row parsed: has the table's format changed?")
        self.assertEqual(len(applied), text.count(", CodeRecordedAsApplied),\n"))

    def test_a_long_list_is_cut_short(self):
        oracle = boot(*((CODE, f"2026_R{n}") for n in range(5)))
        self.assertEqual(checks_boot.ran(boot(), oracle),
                         ["5 code migration(s) it runs on the source did not run: 2026_R0, 2026_R1, 2026_R2; and 2 more"])


class Errors(unittest.TestCase):
    def test_the_blocked_plugin_repository_is_expected(self):
        self.assertEqual(checks_boot.errors(CLEAN), [])

    def test_unexpected_errors_are_counted_and_the_first_quoted(self):
        reasons = checks_boot.errors(FAILED)
        self.assertEqual(len(reasons), 1)
        self.assertTrue(reasons[0].startswith("4 unexpected error(s): [ERR] Microsoft.EntityFrameworkCore"), reasons[0])
        self.assertIn("[FTL] InternalDatabaseMigration: Error: SQLite Error 1", reasons[0])
        self.assertTrue(reasons[0].endswith("; and 1 more"), reasons[0])

    def test_a_time_with_milliseconds_is_an_entry(self):
        # the console template of the logging.default.json a fresh install of 12.2 or later writes
        log = "[20:31:19.252] [INF] [1] Main: starting\n[20:31:19.371] [ERR] [11] Main: broken\n"
        self.assertEqual(checks_boot.errors(log), ["1 unexpected error(s): [ERR] Main: broken"])
        self.assertEqual(checks_boot.migrations(log.replace("Main: starting", "InternalCodeMigration: Perform migration X")),
                         ["migration X failed: Main: broken"])

    def test_a_continuation_line_is_never_an_entry(self):
        log = "[01:00:00] [INF] [1] Main: starting\n[ERR] looks like a level but continues the entry above\n"
        self.assertEqual(checks_boot.errors(log), [])


class CommandLine(unittest.TestCase):
    def run_on(self, mode, text, *oracle):
        with tempfile.NamedTemporaryFile("w", suffix=".log") as log, redirect_stdout(io.StringIO()):
            log.write(text)
            log.flush()
            return checks_boot.main([mode, log.name, *oracle])

    def test_ran_needs_a_readable_oracle_with_entries_and_the_source_s_history(self):
        with tempfile.TemporaryDirectory() as tmp, tempfile.NamedTemporaryFile("w", suffix=".log") as oracle:
            source = Path(tmp) / "jellyfin.db"
            with closing(sqlite3.connect(source)) as db:
                db.executescript("CREATE TABLE __EFMigrationsHistory (MigrationId TEXT, ProductVersion TEXT);"
                                 "INSERT INTO __EFMigrationsHistory VALUES ('2026_Recorded', '10.11.10.0');")
            oracle.write(boot((CODE, "2026_Fix"), (CODE, "2026_Recorded")))
            oracle.flush()
            self.assertEqual(self.run_on("ran", boot((CODE, "2026_Fix")), oracle.name, source), 0)
            self.assertEqual(self.run_on("ran", boot(), oracle.name, source), 1)
            self.assertEqual(self.run_on("ran", boot(), "/nonexistent/oracle.log", source), 2)
            self.assertEqual(self.run_on("ran", boot(), oracle.name, Path(tmp) / "missing.db"), 2)
            with closing(sqlite3.connect(Path(tmp) / "no-history.db")) as db:
                db.execute("CREATE TABLE Users (Id TEXT)")
            # a source with no history (native): its export records a fresh install's, so Jellyfin runs nothing
            self.assertEqual(self.run_on("ran", boot(), oracle.name, Path(tmp) / "no-history.db"), 0)
            self.assertEqual(self.run_on("ran", boot((CODE, "2026_Fix")), oracle.name, Path(tmp) / "no-history.db"), 1)
            with redirect_stderr(io.StringIO()):
                self.assertEqual(self.run_on("ran", boot(), oracle.name), 2)   # no source given
            with tempfile.NamedTemporaryFile("w", suffix=".log") as empty:
                self.assertEqual(self.run_on("ran", boot(), empty.name, source), 2)

    def test_exit_status(self):
        self.assertEqual(self.run_on("migrations", CLEAN), 0)
        self.assertEqual(self.run_on("migrations", FAILED), 1)
        self.assertEqual(self.run_on("errors", FAILED), 1)
        self.assertEqual(self.run_on("migrations", ""), 2)   # an empty capture never passes
        self.assertEqual(self.run_on("errors", "\x1b[32minfo\x1b[0m: not Jellyfin's format\n"), 2)
        with redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
            self.assertEqual(checks_boot.main(["errors", "/nonexistent/jellyfin.log"]), 2)
            self.assertEqual(checks_boot.main(["neither", "x"]), 2)
            self.assertEqual(checks_boot.main([]), 2)


if __name__ == "__main__":
    unittest.main()
