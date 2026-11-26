"""native.py and the native route's rule in checks_boot.py: the drift guard against a fresh install's history (an
unclassified id simulated), the traps the seeding sets, the decoy library.db.old, and a source without a history
expecting Jellyfin to run nothing."""

from contextlib import closing, redirect_stdout
import io
import sqlite3
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.append(str(Path(__file__).resolve().parent.parent))
import checks_boot  # noqa: E402
import native  # noqa: E402

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
#: A fresh Jellyfin 12.4 install's history, as the export's own test has it.
FRESH = dict(line.split("\t") for line in
             (REPO / "crates/ferrofin-db/tests/data/jellyfin-12.4-fresh-history.tsv").read_text().splitlines())
NEW_ID = "20270101000000_AddSomethingNew"


def database(path, history=None, statements=()):
    """A SQLite file at path with the history {id: version} (none: no history table) and the statements run."""
    with closing(sqlite3.connect(path)) as db, db:
        if history is not None:
            db.execute('CREATE TABLE "__EFMigrationsHistory" ("MigrationId" TEXT PRIMARY KEY, "ProductVersion" TEXT)')
            db.executemany('INSERT INTO "__EFMigrationsHistory" VALUES (?, ?)', history.items())
        for statement in statements:
            db.execute(statement)
    return path


class Drift(unittest.TestCase):
    def test_the_export_of_a_fresh_install_s_history_passes(self):
        self.assertEqual(len(FRESH), 105)
        self.assertEqual(native.drift(FRESH, dict(FRESH)), [])

    def test_an_id_a_new_release_adds_fails_by_name(self):
        """B11, simulated: the release records an id the exporter does not classify, so the export lacks it."""
        reasons = native.drift({**FRESH, NEW_ID: "12.5.0.0"}, dict(FRESH))
        self.assertEqual(len(reasons), 1)
        self.assertIn(NEW_ID, reasons[0])
        self.assertIn("does not classify", reasons[0])
        self.assertIn("JELLYFIN_MIGRATIONS", reasons[0])

    def test_an_id_the_release_no_longer_records_and_another_version_fail(self):
        export = dict(FRESH)
        export["20990101000000_Gone"] = "12.4.0.0"
        export["20200514181226_AddActivityLog"] = "9.0.11"
        reasons = native.drift(FRESH, export)
        self.assertEqual(len(reasons), 2)
        self.assertIn("1 id(s) the export records that a fresh install does not: 20990101000000_Gone", reasons[0])
        self.assertIn("20200514181226_AddActivityLog (9.0.11, not 10.0.14)", reasons[1])

    def test_a_fresh_install_with_no_history_never_passes(self):
        self.assertEqual(len(native.drift({}, {})), 1)

    def test_the_command_reads_both_databases(self):
        with tempfile.TemporaryDirectory() as tmp:
            fresh = database(Path(tmp, "fresh.db"), {**FRESH, NEW_ID: "12.5.0.0"})
            export = database(Path(tmp, "export.db"), FRESH)
            out = io.StringIO()
            with redirect_stdout(out):
                self.assertEqual(native.main(["drift", "--fresh", str(fresh), "--export", str(export)]), 1)
            self.assertIn(NEW_ID, out.getvalue())
            with redirect_stdout(io.StringIO()):
                self.assertEqual(native.main(["drift", "--fresh", str(export), "--export", str(export)]), 0)
                self.assertEqual(native.main(["drift", "--fresh", str(Path(tmp, "none.db")),
                                              "--export", str(export)]), 2)


class FakeApi:
    """The settings documents of a server, as synthetic_fixture.Api reads and writes them; `keeps` False drops
    every write."""

    def __init__(self, keeps=True):
        self.documents = {"network": {"InternalHttpPort": 8096}, "encoding": {"EnableThrottling": False}}
        self.keeps = keeps

    def get(self, path):
        return dict(self.documents[path.rsplit("/", 1)[1]])

    def post(self, path, body):
        if self.keeps:
            self.documents[path.rsplit("/", 1)[1]] = body


class Traps(unittest.TestCase):
    def test_each_trap_is_set_away_from_its_default(self):
        api = FakeApi()
        native.set_traps(api)
        self.assertEqual(api.documents, {"network": {"InternalHttpPort": 8097}, "encoding": {"EnableThrottling": True}})

    def test_a_setting_the_server_did_not_keep_fails(self):
        with self.assertRaises(native.CheckError):
            native.set_traps(FakeApi(keeps=False))


class Decoy(unittest.TestCase):
    SOURCE = (
        'CREATE TABLE "BaseItems" ("Id" TEXT, "Type" TEXT, "Name" TEXT)',
        f"INSERT INTO \"BaseItems\" VALUES ('B0000000-0000-0000-0000-000000000002', '{native.MOVIE}', 'Blue')",
        f"INSERT INTO \"BaseItems\" VALUES ('A0000000-0000-0000-0000-000000000001', '{native.MOVIE}', 'Amber')",
        'CREATE TABLE "Users" ("Id" TEXT, "InternalId" INTEGER)',
        "INSERT INTO \"Users\" VALUES ('U1', 1), ('U2', 2)",
        'CREATE TABLE "Permissions" ("UserId" TEXT, "Kind" INTEGER, "Value" INTEGER)',
        "INSERT INTO \"Permissions\" VALUES ('U1', 1, 1), ('U2', 0, 1)",
    )

    def test_it_names_the_first_movie_a_folder_and_detaches_a_row_of_the_administrator_s(self):
        with tempfile.TemporaryDirectory() as tmp:
            Path(tmp, "data").mkdir()
            database(Path(tmp, "data/jellyfin.db"), statements=self.SOURCE)
            native.decoy(tmp)
            with closing(sqlite3.connect(Path(tmp, native.DECOY))) as db:
                # what ReseedFolderFlag and MigrateLibraryUserData select
                self.assertEqual(db.execute("SELECT guid FROM TypedBaseItems WHERE IsFolder = true").fetchall(),
                                 [("A0000000-0000-0000-0000-000000000001",)])
                self.assertEqual(db.execute(
                    "SELECT key, userId, played, playCount FROM UserDatas WHERE NOT EXISTS (SELECT 1 FROM "
                    "TypedBaseItems WHERE TypedBaseItems.UserDataKey = UserDatas.key)").fetchall(),
                    [("decoy-detached", 2, 1, 1)])
            with self.assertRaises(native.CheckError):   # never over one already there
                native.decoy(tmp)


class NoHistory(unittest.TestCase):
    def test_a_source_without_a_history_expects_jellyfin_to_run_nothing(self):
        with tempfile.TemporaryDirectory() as tmp:
            source = database(Path(tmp, "ferrofin.db"))
            self.assertIsNone(checks_boot.recorded(source))
        oracle = "[01:00:00] [INF] [1] InternalCodeMigration: Perform migration 2025_AddDefaultCastReceivers\n"
        self.assertEqual(checks_boot.ran("[01:00:00] [INF] [1] Main: started\n", oracle, None), [])
        ran = checks_boot.ran(oracle, oracle, None)
        self.assertEqual(ran, ["1 code migration(s) ran that it does not run on the source: 2025_AddDefaultCastReceivers"])
        # with a history, the oracle's routines are expected as before
        self.assertEqual(checks_boot.ran(oracle, oracle, set()), [])


if __name__ == "__main__":
    unittest.main()
