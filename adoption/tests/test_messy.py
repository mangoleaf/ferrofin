"""The messy-row shapes on a scratch SQLite database: each seeder, the end-state reader, and the
comparison against Jellyfin 12.2's record and the written divergences (no docker, no fixtures)."""

import contextlib
from contextlib import closing
import json
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
import uuid

sys.path.insert(0, str(Path(__file__).parents[1]))
import messy
from metadata import CheckError
from synthetic_fixture import USERS, seed_id

ADULT = "5136F4F7-6903-1423-B8DF-4F11C37566A5"
ITEM = "1559654E-1FB0-5780-0C66-00CA97F14DBB"
OTHER_ITEM = "2A59654E-1FB0-5780-0C66-00CA97F14DBB"
EXTRA = "3B36F4F7-6903-1423-B8DF-4F11C37566A5"
PRIVATE = "4C36F4F7-6903-1423-B8DF-4F11C37566A5"
SCHEMA = """
CREATE TABLE Users (Id TEXT NOT NULL PRIMARY KEY);
CREATE TABLE BaseItems (Id TEXT NOT NULL PRIMARY KEY, Type TEXT NOT NULL, Name TEXT, Path TEXT, OwnerId TEXT,
    ExtraType INTEGER NULL, PresentationUniqueKey TEXT, DateCreated TEXT);
CREATE TABLE UserData (ItemId TEXT NOT NULL, UserId TEXT NOT NULL, CustomDataKey TEXT NOT NULL,
    Played INTEGER NOT NULL, PRIMARY KEY (ItemId, UserId, CustomDataKey));
CREATE TABLE Permissions (Id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT, Kind INTEGER NOT NULL,
    RowVersion INTEGER NOT NULL, UserId TEXT NOT NULL, Value INTEGER NOT NULL);
CREATE UNIQUE INDEX IX_Permissions_UserId_Kind ON Permissions (UserId, Kind);
CREATE TABLE Preferences (Id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT, Kind INTEGER NOT NULL,
    RowVersion INTEGER NOT NULL, UserId TEXT NOT NULL, Value TEXT NOT NULL);
CREATE TABLE Devices (Id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT, UserId TEXT NOT NULL, AccessToken TEXT NOT NULL,
    DeviceId TEXT NOT NULL);
CREATE TABLE DisplayPreferences (Id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT, UserId TEXT NOT NULL,
    Client TEXT NOT NULL, ShowSidebar INTEGER NOT NULL DEFAULT 0, ShowBackdrop INTEGER NOT NULL DEFAULT 0,
    ScrollDirection INTEGER NOT NULL DEFAULT 0, SkipForwardLength INTEGER NOT NULL DEFAULT 0,
    SkipBackwardLength INTEGER NOT NULL DEFAULT 0, ChromecastVersion INTEGER NOT NULL DEFAULT 0,
    EnableNextVideoInfoOverlay INTEGER NOT NULL DEFAULT 0,
    ItemId TEXT NOT NULL);
CREATE UNIQUE INDEX IX_DisplayPreferences ON DisplayPreferences (UserId, ItemId, Client);
"""
MANIFEST = {"users": {name: str(uuid.UUID(ADULT)) for name in USERS}, "private_movie": PRIVATE}


def populate(db):
    """A scratch database holding the rows the seeders copy: one user, two movies, an extra."""
    with closing(sqlite3.connect(db)) as conn:
        conn.executescript(SCHEMA + f"""
            INSERT INTO Users VALUES ('{ADULT}');
            INSERT INTO BaseItems VALUES ('{ITEM}', 'MediaBrowser.Controller.Entities.Movies.Movie', 'Movie', '/media/movies/a/a.mkv', NULL, NULL, 'k1', '2026-10-20 04:22:14.4161545');
            INSERT INTO BaseItems VALUES ('{PRIVATE}', 'MediaBrowser.Controller.Entities.Movies.Movie', 'Private', '/media/movies/p/p.mkv', NULL, NULL, 'k2', '2026-10-20 04:22:15.4161545');
            INSERT INTO BaseItems VALUES ('{EXTRA}', '{messy.VIDEO_TYPE}', 'Making of', '/media/movies/a/extras/x.mkv', '{ITEM}', 3, 'k3', '2026-10-20 04:22:16.4161545');
            INSERT INTO UserData VALUES ('{ITEM}', '{ADULT}', 'a', 1), ('{PRIVATE}', '{ADULT}', 'b', 0), ('{EXTRA}', '{ADULT}', 'c', 0);
            INSERT INTO Permissions (Kind, RowVersion, UserId, Value) VALUES (0, 0, '{ADULT}', 1), (1, 0, '{ADULT}', 0);
            INSERT INTO Preferences (Kind, RowVersion, UserId, Value) VALUES (2, 0, '{ADULT}', 'x');
            INSERT INTO Devices (UserId, AccessToken, DeviceId) VALUES ('{ADULT}', 'tok', 'dev');
            INSERT INTO DisplayPreferences (UserId, Client, ItemId) VALUES ('{ADULT}', 'emby', '00000000-0000-0000-0000-000000000000');
        """)


class Scratch(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.db = str(Path(directory.name) / "jellyfin.db")
        populate(self.db)

    def conn(self):
        conn = sqlite3.connect(self.db)
        self.addCleanup(conn.close)
        return conn


class Seeders(Scratch):
    def test_deleted_user_rows_name_a_user_that_is_not_there(self):
        entry = messy.seed_messy(self.db, MANIFEST, ("deleted_user",))["shapes"]["deleted_user"]
        ghost = entry["user"]
        self.assertEqual(entry["rows"], {"UserData": 2, "Permissions": 2, "Preferences": 1, "Devices": 1, "DisplayPreferences": 1})
        conn = self.conn()
        self.assertEqual(conn.execute("SELECT COUNT(*) FROM Users WHERE lower(replace(Id, '-', '')) = ?", (ghost,)).fetchone()[0], 0)
        state = messy.shape_state(conn, {"shapes": {"deleted_user": entry}})["deleted_user"]
        self.assertEqual(state, entry["rows"])
        # the cloned rows are written in the stored spelling (dashed, upper case)
        self.assertEqual(conn.execute("SELECT UserId FROM Devices WHERE DeviceId = 'synthetic-messy-deleted-device'").fetchone()[0],
                         str(uuid.UUID(ghost)).upper())

    def test_the_account_the_rows_were_copied_from_is_untouched(self):
        messy.seed_messy(self.db, MANIFEST, ("deleted_user",))
        conn = self.conn()
        self.assertEqual(conn.execute("SELECT COUNT(*) FROM Permissions WHERE UserId = ?", (ADULT,)).fetchone()[0], 2)
        self.assertEqual(conn.execute("SELECT COUNT(*) FROM UserData WHERE UserId = ?", (ADULT,)).fetchone()[0], 3)

    def test_nothing_to_copy_is_a_build_failure_not_a_silent_skip(self):
        with closing(sqlite3.connect(self.db)) as conn, conn:
            conn.execute("DELETE FROM Devices")
        with self.assertRaisesRegex(CheckError, "nothing to copy for Devices"):
            messy.seed_messy(self.db, MANIFEST, ("deleted_user",))

    def test_a_fixture_with_no_display_preferences_gets_one_written_for_the_deleted_user(self):
        with closing(sqlite3.connect(self.db)) as conn, conn:
            conn.execute("DELETE FROM DisplayPreferences")
        entry = messy.seed_messy(self.db, MANIFEST, ("deleted_user",))["shapes"]["deleted_user"]
        self.assertEqual(entry["rows"]["DisplayPreferences"], 1)
        row = self.conn().execute("SELECT UserId, Client, ItemId FROM DisplayPreferences").fetchone()
        self.assertEqual((uuid.UUID(row[0]).hex, row[1]), (entry["user"], "synthetic-messy"))
        # a user-wide preference: the empty Guid, in the spelling the other ids use
        self.assertEqual(row[2], str(uuid.UUID(int=0)).upper())

    def test_dangling_owner_extra_names_an_owner_no_row_has(self):
        entry = messy.seed_messy(self.db, MANIFEST, ("dangling_owner_extra",))["shapes"]["dangling_owner_extra"]
        conn = self.conn()
        row = conn.execute("SELECT Id, OwnerId, ExtraType, Path FROM BaseItems WHERE lower(replace(Id, '-', '')) = ?",
                           (entry["extra"],)).fetchone()
        owner = uuid.UUID(row[1]).hex
        self.assertEqual(owner, entry["owner"])
        self.assertEqual(conn.execute("SELECT COUNT(*) FROM BaseItems WHERE lower(replace(Id, '-', '')) = ?", (owner,)).fetchone()[0], 0)
        self.assertEqual(row[2], 3)
        self.assertEqual(messy.shape_state(conn, {"shapes": {"dangling_owner_extra": entry}})["dangling_owner_extra"],
                         {"exists": True, "owner": "dangling"})

    def test_duplicate_paths_share_a_path_and_the_copy_is_the_older_row(self):
        entry = messy.seed_messy(self.db, MANIFEST, ("duplicate_paths",))["shapes"]["duplicate_paths"]
        conn = self.conn()
        rows = conn.execute("SELECT Id, DateCreated FROM BaseItems WHERE Path = ? ORDER BY DateCreated", (entry["path"],)).fetchall()
        self.assertEqual(len(rows), 2)
        self.assertEqual(uuid.UUID(rows[0][0]).hex, entry["duplicate"])
        self.assertEqual(uuid.UUID(rows[1][0]).hex, entry["original"])
        self.assertEqual(messy.shape_state(conn, {"shapes": {"duplicate_paths": entry}})["duplicate_paths"],
                         {"original": True, "duplicate": True})

    def test_the_manifest_names_every_shape_and_all_shapes_can_share_one_database(self):
        entry = messy.seed_messy(self.db, MANIFEST)
        self.assertEqual(tuple(entry["shapes"]), messy.SHAPES)

    def test_ids_are_fixed_so_a_rebuild_writes_the_same_rows(self):
        first = messy.seed_messy(self.db, MANIFEST, ("dangling_owner_extra", "duplicate_paths"))
        self.assertEqual(first["shapes"]["duplicate_paths"]["duplicate"], seed_id("messy-duplicate").hex)
        self.assertEqual(first["shapes"]["dangling_owner_extra"]["extra"], seed_id("messy-extra").hex)


class EndState(Scratch):
    def test_each_way_an_extras_owner_can_read(self):
        entry = messy.seed_messy(self.db, MANIFEST, ("dangling_owner_extra",))["shapes"]["dangling_owner_extra"]
        wanted = {"shapes": {"dangling_owner_extra": entry}}
        extra = "lower(replace(Id, '-', '')) = '" + entry["extra"] + "'"
        for owner, kind in ((None, "none"), (f"'{ITEM}'", "other"), (f"'{messy.PLACEHOLDER}'", "placeholder"),
                            (f"'{str(uuid.UUID(entry['extra'])).upper()}'", "self")):
            with closing(sqlite3.connect(self.db)) as conn, conn:
                conn.execute(f"UPDATE BaseItems SET OwnerId = {owner or 'NULL'} WHERE {extra}")
            with self.subTest(kind):
                self.assertEqual(messy.read_state(self.db, wanted)["dangling_owner_extra"]["owner"], kind)
        with closing(sqlite3.connect(self.db)) as conn, conn:
            conn.execute(f"DELETE FROM BaseItems WHERE {extra}")
        self.assertEqual(messy.read_state(self.db, wanted)["dangling_owner_extra"], {"exists": False, "owner": None})

    def test_rows_deleted_by_an_adoption_show_in_the_state(self):
        entry = messy.seed_messy(self.db, MANIFEST)
        ghost = entry["shapes"]["deleted_user"]["user"]
        with closing(sqlite3.connect(self.db)) as conn, conn:
            for table in messy.USER_TABLES:
                conn.execute(f"DELETE FROM {table} WHERE lower(replace(UserId, '-', '')) = ?", (ghost,))
            conn.execute("DELETE FROM BaseItems WHERE lower(replace(Id, '-', '')) = ?", (entry["shapes"]["duplicate_paths"]["duplicate"],))
        state = messy.read_state(self.db, entry)
        self.assertEqual(state["deleted_user"], {table: 0 for table in messy.USER_TABLES})
        self.assertEqual(state["duplicate_paths"], {"original": True, "duplicate": False})


class Comparison(unittest.TestCase):
    ENTRY = {"shapes": {"deleted_user": {}, "duplicate_paths": {}}}
    GONE = {table: 0 for table in messy.USER_TABLES}
    KEPT = {table: 1 for table in messy.USER_TABLES}

    def setUp(self):
        # These test the mechanism: the real divergences (WrittenDivergences) must not leak in.
        patcher = mock.patch.dict(messy.DIVERGENCES, clear=True)
        patcher.start()
        self.addCleanup(patcher.stop)

    def oracle(self, **shapes):
        return {"shapes": {name: record for name, record in shapes.items()}}

    def booted(self, shape, state):
        return {"outcome": "booted", "state": {shape: state}}

    def test_the_state_jellyfin_12_2_left_is_the_expectation(self):
        oracle = self.oracle(deleted_user=self.booted("deleted_user", self.GONE),
                             duplicate_paths=self.booted("duplicate_paths", {"original": True, "duplicate": False}))
        good = {"deleted_user": self.GONE, "duplicate_paths": {"original": True, "duplicate": False}}
        self.assertEqual(messy.compare(good, self.ENTRY, oracle, "10.11"), [])
        bad = {**good, "deleted_user": self.KEPT}
        failures = messy.compare(bad, self.ENTRY, oracle, "10.11")
        self.assertEqual(len(failures), 1)
        self.assertIn("deleted_user", failures[0])
        self.assertIn("Jellyfin 12.2", failures[0])

    def test_a_written_divergence_replaces_the_oracle_for_that_shape_and_lineage_only(self):
        oracle = self.oracle(deleted_user=self.booted("deleted_user", self.KEPT),
                             duplicate_paths=self.booted("duplicate_paths", {"original": True, "duplicate": True}))
        ferrofin = {"deleted_user": self.GONE, "duplicate_paths": {"original": True, "duplicate": True}}
        with mock.patch.dict(messy.DIVERGENCES, {("deleted_user", "10.11"): (self.GONE, "0044 deletes it")}):
            self.assertEqual(messy.compare(ferrofin, self.ENTRY, oracle, "10.11"), [])
            # the other lineage has no divergence written: Jellyfin 12.2's kept rows are the expectation
            self.assertTrue(messy.compare(ferrofin, self.ENTRY, oracle, "12.x"))
            # a divergence names the state Ferrofin must reach: anything else still fails
            changed = {**ferrofin, "deleted_user": {**self.GONE, "Devices": 1}}
            failures = messy.compare(changed, self.ENTRY, oracle, "10.11")
            self.assertEqual(len(failures), 1)
            self.assertIn("written divergence: 0044 deletes it", failures[0])

    def test_a_shape_jellyfin_refuses_needs_a_written_expectation(self):
        oracle = self.oracle(deleted_user={"outcome": "refused", "reason": "FOREIGN KEY constraint failed", "state": {}},
                             duplicate_paths=self.booted("duplicate_paths", {"original": True, "duplicate": False}))
        ferrofin = {"deleted_user": self.GONE, "duplicate_paths": {"original": True, "duplicate": False}}
        failures = messy.compare(ferrofin, self.ENTRY, oracle, "10.11")
        self.assertEqual(len(failures), 1)
        self.assertIn("refused this shape (FOREIGN KEY constraint failed)", failures[0])
        with mock.patch.dict(messy.DIVERGENCES, {("deleted_user", "10.11"): (self.GONE, "Ferrofin boots it")}):
            self.assertEqual(messy.compare(ferrofin, self.ENTRY, oracle, "10.11"), [])

    def test_check_reads_the_fixtures_manifest_and_oracle_record(self):
        with tempfile.TemporaryDirectory() as directory:
            fixture = Path(directory)
            db = fixture / "jellyfin.db"
            with closing(sqlite3.connect(db)) as conn:
                conn.executescript(SCHEMA)
            entry = {"lineage": "test", "shapes": {"duplicate_paths": {"path": "/p", "original": "a" * 32, "duplicate": "b" * 32}}}
            (fixture / "messy.json").write_text(json.dumps(entry))
            (fixture / "messy-oracle.json").write_text(json.dumps(self.oracle(
                duplicate_paths=self.booted("duplicate_paths", {"original": False, "duplicate": False}))))
            with mock.patch.object(messy, "SHAPES", ("duplicate_paths",)):
                self.assertEqual(messy.check(db, fixture), [])
                (fixture / "messy-oracle.json").write_text(json.dumps(self.oracle(
                    duplicate_paths=self.booted("duplicate_paths", {"original": True, "duplicate": False}))))
                self.assertEqual(len(messy.check(db, fixture)), 1)

    def test_a_fixture_built_with_other_shapes_is_refused_rather_than_partly_checked(self):
        with tempfile.TemporaryDirectory() as directory:
            fixture = Path(directory)
            (fixture / "messy.json").write_text(json.dumps({"lineage": "test", "shapes": {"deleted_user": {}}}))
            (fixture / "messy-oracle.json").write_text("{}")
            with self.assertRaisesRegex(CheckError, "built with shapes deleted_user, not deleted_user, dangling_owner_extra"):
                messy.check(fixture / "jellyfin.db", fixture)


class OracleRunner(unittest.TestCase):
    """run_oracle_server and refusal_reason with docker and the Jellyfin API replaced."""

    def fake_docker(self, running, log=""):
        calls = []

        def run(command, **kwargs):
            calls.append(command)
            out = {"inspect": f"{running}\n", "logs": log}.get(command[1], "")
            return subprocess.CompletedProcess(command, 0, out, "")
        return run, calls

    def server(self, running, api=None, log="", **patches):
        run, calls = self.fake_docker(running, log)
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        config = Path(directory.name) / "config"
        config.mkdir()
        with contextlib.ExitStack() as stack:
            stack.enter_context(mock.patch.object(messy.subprocess, "run", run))
            stack.enter_context(mock.patch.object(messy, "Api", return_value=api or mock.MagicMock()))
            stack.enter_context(mock.patch.object(messy.time, "sleep"))
            for name, value in patches.items():
                stack.enter_context(mock.patch.object(messy, name, value))
            return messy.run_oracle_server(config, config, 18130), calls

    def test_a_running_container_that_answers_booted(self):
        result, calls = self.server("true")
        self.assertEqual(result, {"outcome": "booted", "reason": ""})
        self.assertEqual([call[1] for call in calls if call[0] == "docker"][-3:], ["stop", "logs", "rm"])  # always stopped, logged and removed

    def test_a_container_that_exited_is_a_refusal_naming_its_last_foreign_key_line(self):
        log = "[INF] starting\nfail: warning only\n[ERR] FOREIGN KEY constraint failed: first\n[ERR] FOREIGN KEY constraint failed: last\n"
        result, _ = self.server("false", log=log)
        self.assertEqual(result["outcome"], "refused")
        self.assertEqual(result["reason"], "[ERR] FOREIGN KEY constraint failed: last")

    def test_a_refusal_with_no_known_marker_says_so(self):
        result, _ = self.server("false", log="nothing useful\n")
        self.assertEqual(result, {"outcome": "refused", "reason": "no error line in the log"})

    def test_a_reason_is_cut_to_the_documented_length(self):
        result, _ = self.server("false", log="[ERR] " + "x" * 500)
        self.assertEqual(len(result["reason"]), messy.REASON_CHARS)

    def test_a_container_that_is_slow_but_running_is_an_error_not_a_refusal(self):
        api = mock.MagicMock()
        api.get.side_effect = OSError("not yet")
        with self.assertRaisesRegex(CheckError, "did not answer within 0 s and did not exit"):
            self.server("true", api=api, ORACLE_READY_S=0)


class RecordAndBuild(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        (self.root / "media").mkdir()
        for name in ("jellyfin-10.11.11", "jellyfin-12.2-from-121"):
            source = self.root / name
            (source / "data").mkdir(parents=True)
            populate(source / "data/jellyfin.db")
            (source / "synthetic-ready.json").write_text("{}")
            (source / "synthetic.json").write_text(json.dumps(MANIFEST))
        self.boots = []

    def oracle(self, command, **kwargs):
        if command[:2] == ["docker", "pull"] or command[:3] == ["docker", "image", "inspect"]:
            return subprocess.CompletedProcess(command, 0, "sha256:image\n", "")
        raise AssertionError(command)

    def booted(self, outcome="booted", combined=None):
        def run(config, media, port):
            self.boots.append(Path(config).name)
            if combined and Path(config).name.endswith("-all"):
                return dict(combined)
            return {"outcome": outcome, "reason": ""}
        return run

    def record(self, **kwargs):
        with mock.patch.object(messy.subprocess, "run", self.oracle), mock.patch.object(messy, "run_oracle_server", self.booted(**kwargs)):
            return messy.record_oracle(self.root, self.root / "jellyfin-10.11.11", messy.SHAPES, MANIFEST, 18130)

    def test_each_shape_gets_a_clean_copy_of_its_own_and_one_more_carries_them_all(self):
        record = self.record()
        self.assertEqual([name.split("jellyfin-10.11.11-")[1] for name in self.boots], [*messy.SHAPES, "all"])
        self.assertEqual(set(record["shapes"]), set(messy.SHAPES))
        self.assertEqual(record["image"], messy.ORACLE_IMAGE)
        self.assertEqual(record["image_id"], "sha256:image")
        # the single-shape copy carries only its shape, and the scratch is removed afterwards
        self.assertEqual(set(record["shapes"]["deleted_user"]["state"]), {"deleted_user"})
        self.assertEqual(list((self.root / "work").iterdir()), [])

    def test_a_combined_run_that_differs_from_the_single_runs_is_an_error(self):
        with self.assertRaisesRegex(CheckError, "handled .* differently with all the shapes present"):
            self.record(combined={"outcome": "refused", "reason": "x"})

    def test_a_failed_boot_leaves_no_scratch_copy(self):
        def boom(config, media, port):
            raise CheckError("docker went away")
        with mock.patch.object(messy.subprocess, "run", self.oracle), mock.patch.object(messy, "run_oracle_server", boom):
            with self.assertRaises(CheckError):
                messy.record_oracle(self.root, self.root / "jellyfin-10.11.11", messy.SHAPES, MANIFEST, 18130)
        self.assertEqual(list((self.root / "work").iterdir()), [])

    def test_a_failed_build_removes_its_derived_copy_and_leaves_the_clean_fixtures(self):
        with mock.patch.object(messy, "record_oracle", side_effect=CheckError("no oracle")):
            with self.assertRaisesRegex(CheckError, "no oracle"):
                messy.build(self.root, 18130)
        self.assertFalse((self.root / "jellyfin-10.11.11-messy").exists())
        self.assertTrue((self.root / "jellyfin-10.11.11/data/jellyfin.db").exists())

    def test_a_finished_build_is_kept_and_a_second_build_does_nothing(self):
        record = {"shapes": {shape: {"outcome": "booted"} for shape in messy.SHAPES}}
        with mock.patch.object(messy, "record_oracle", return_value=record) as recorded:
            messy.build(self.root, 18130)
            self.assertEqual(recorded.call_count, 2)
            messy.build(self.root, 18130)
            self.assertEqual(recorded.call_count, 2)
        derived = json.loads((self.root / "jellyfin-12.2-messy/messy.json").read_text())
        self.assertEqual((derived["lineage"], derived["source"], set(derived["shapes"])),
                         ("12.x", "jellyfin-12.2-from-121", set(messy.SHAPES)))

    def test_a_built_fixture_with_other_shapes_is_refused_not_reused(self):
        record = {"shapes": {shape: {"outcome": "booted"} for shape in messy.SHAPES}}
        with mock.patch.object(messy, "record_oracle", return_value=record):
            messy.build(self.root, 18130)
        with mock.patch.object(messy, "SHAPES", ("deleted_user",)):
            with self.assertRaisesRegex(CheckError, "built with shapes .*build into a new fixtures directory"):
                messy.build(self.root, 18130)

    def test_a_missing_source_fixture_is_named(self):
        (self.root / "jellyfin-10.11.11/synthetic-ready.json").unlink()
        with self.assertRaisesRegex(CheckError, "jellyfin-10.11.11 is not built"):
            messy.build(self.root, 18130)


class WrittenDivergences(unittest.TestCase):
    KEYS = {"deleted_user": set(messy.USER_TABLES), "dangling_owner_extra": {"exists", "owner"},
            "duplicate_paths": {"original", "duplicate"}}

    def test_each_divergence_names_a_seeded_shape_a_derived_lineage_and_a_whole_state_with_a_reason(self):
        lineages = {lineage for _, _, lineage in messy.DERIVED}
        self.assertTrue(messy.DIVERGENCES)
        for (shape, lineage), (state, why) in messy.DIVERGENCES.items():
            with self.subTest((shape, lineage)):
                self.assertIn(shape, messy.SHAPES)
                self.assertIn(lineage, lineages)
                self.assertEqual(set(state), self.KEYS[shape])
                self.assertGreater(len(why), 20)


class Helpers(unittest.TestCase):
    def test_spelled_follows_the_stored_id(self):
        value = uuid.UUID("0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9")
        self.assertEqual(messy.spelled("0A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9", value), str(value).upper())
        self.assertEqual(messy.spelled("0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9", value), str(value))
        self.assertEqual(messy.spelled("0A1B2C3D4E5F60718293A4B5C6D7E8F9", value), value.hex.upper())
        self.assertEqual(messy.spelled("0a1b2c3d4e5f60718293a4b5c6d7e8f9", value), value.hex)

    def test_each_derived_fixture_copies_a_fixture_the_builder_makes_and_a_run_row_adopts(self):
        from synthetic import VERSIONS
        built = {name for name, _, _ in VERSIONS}
        run_sh = (Path(__file__).parents[1] / "run.sh").read_text()
        for name, source, _ in messy.DERIVED:
            with self.subTest(name):
                self.assertIn(source, built)
                self.assertIn(f"|{name}|", run_sh)   # the MESSY_TABLE row that adopts it


if __name__ == "__main__":
    unittest.main()
