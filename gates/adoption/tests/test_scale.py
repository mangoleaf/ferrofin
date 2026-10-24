"""The scale fixture's cloning, its survival check and its measurements (adoption/scale.py)."""

from contextlib import closing
import json
import os
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parents[1]))
import scale
from metadata import CheckError

MOVIE, EPISODE, FOLDER = scale.TEMPLATE_TYPES[0], scale.TEMPLATE_TYPES[1], "MediaBrowser.Controller.Entities.Folder"
ZERO = "00000000-0000-0000-0000-000000000000"
USERS = ("AAAAAAAA-0000-0000-0000-000000000001", "AAAAAAAA-0000-0000-0000-000000000002")

SCHEMA = """
CREATE TABLE Users (Id TEXT PRIMARY KEY);
CREATE TABLE BaseItems (Id TEXT PRIMARY KEY, Type TEXT, Path TEXT, OwnerId TEXT, PrimaryVersionId TEXT,
    ExtraType INTEGER, Name TEXT, SortName TEXT, CleanName TEXT, PresentationUniqueKey TEXT,
    ParentId TEXT REFERENCES BaseItems(Id));
CREATE TABLE AncestorIds (ItemId TEXT REFERENCES BaseItems(Id), ParentItemId TEXT REFERENCES BaseItems(Id),
    PRIMARY KEY (ItemId, ParentItemId));
CREATE TABLE MediaStreamInfos (ItemId TEXT REFERENCES BaseItems(Id), StreamIndex INTEGER, Codec TEXT,
    PRIMARY KEY (ItemId, StreamIndex));
CREATE TABLE BaseItemImageInfos (Id TEXT PRIMARY KEY, ItemId TEXT REFERENCES BaseItems(Id), Path TEXT);
CREATE TABLE ItemValues (ItemValueId TEXT PRIMARY KEY, Value TEXT);
CREATE TABLE ItemValuesMap (ItemValueId TEXT, ItemId TEXT REFERENCES BaseItems(Id), PRIMARY KEY (ItemValueId, ItemId));
CREATE TABLE UserData (ItemId TEXT REFERENCES BaseItems(Id), UserId TEXT, CustomDataKey TEXT, Played INTEGER,
    PlaybackPositionTicks INTEGER, PRIMARY KEY (ItemId, UserId, CustomDataKey));
"""


def library(path, owned_extra=True):
    """A small scanned library: a folder, two movies, an episode, and an extra owned by a movie."""
    with closing(sqlite3.connect(path)) as conn:
        conn.executescript(SCHEMA)
        conn.executemany("INSERT INTO Users VALUES (?)", [(user,) for user in USERS])
        items = [("F0000000-0000-0000-0000-000000000000", FOLDER, "/media/movies", ZERO, None, None, "Movies"),
                 ("A0000000-0000-0000-0000-000000000001", MOVIE, "/media/movies/Alpha/Alpha.mkv", ZERO, None, None, "Alpha"),
                 ("A0000000-0000-0000-0000-000000000002", MOVIE, "/media/movies/Beta/Beta.mkv", None, None, None, "Beta"),
                 ("A0000000-0000-0000-0000-000000000003", EPISODE, "/media/shows/Show/S01/E01.mkv", ZERO, None, 0, "E1")]
        if owned_extra:
            items.append(("A0000000-0000-0000-0000-000000000004", MOVIE, "/media/movies/Alpha/extras/Trailer.mkv",
                          "A0000000-0000-0000-0000-000000000001", None, 1, "Trailer"))
        for ident, kind, path_, owner, primary, extra, name in items:
            conn.execute("INSERT INTO BaseItems VALUES (?,?,?,?,?,?,?,?,?,?,?)",
                         (ident, kind, path_, owner, primary, extra, name, name, name.lower(), ident.lower(),
                          "F0000000-0000-0000-0000-000000000000" if ident[0] == "A" else None))
        for ident in ("A0000000-0000-0000-0000-000000000001", "A0000000-0000-0000-0000-000000000002",
                      "A0000000-0000-0000-0000-000000000003"):
            conn.execute("INSERT INTO AncestorIds VALUES (?, 'F0000000-0000-0000-0000-000000000000')", (ident,))
            for index, codec in enumerate(("h264", "aac")):
                conn.execute("INSERT INTO MediaStreamInfos VALUES (?,?,?)", (ident, index, codec))
            conn.execute("INSERT INTO BaseItemImageInfos VALUES (?,?,?)", ("B" + ident[1:], ident, "/cache/primary.jpg"))
            conn.execute("INSERT INTO ItemValues VALUES ('C0000000-0000-0000-0000-000000000001', 'Drama') "
                         "ON CONFLICT DO NOTHING")
            conn.execute("INSERT INTO ItemValuesMap VALUES ('C0000000-0000-0000-0000-000000000001', ?)", (ident,))
        conn.execute("INSERT INTO UserData VALUES ('A0000000-0000-0000-0000-000000000001', ?, 'k1', 1, 0)", (USERS[0],))
        conn.execute("INSERT INTO UserData VALUES ('A0000000-0000-0000-0000-000000000002', ?, 'k2', 0, 900)", (USERS[1],))
        conn.commit()


def rows(path, sql, *params):
    with closing(sqlite3.connect(path)) as conn:
        return conn.execute(sql, params).fetchall()


class CloneTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.db = Path(self.dir.name) / "jellyfin.db"
        library(self.db)

    def tearDown(self):
        self.dir.cleanup()

    def clone(self, items=100):
        with closing(sqlite3.connect(self.db)) as conn:
            conn.execute("PRAGMA foreign_keys = OFF")
            with conn:
                return scale.clone_items(conn, items)

    def test_the_library_grows_to_exactly_the_asked_for_size(self):
        before = rows(self.db, "SELECT COUNT(*) FROM BaseItems")[0][0]
        added = self.clone(100)
        self.assertEqual(rows(self.db, "SELECT COUNT(*) FROM BaseItems")[0][0], 100)
        self.assertEqual(added["BaseItems"], 100 - before)

    def test_only_media_items_with_no_owner_are_repeated_round_robin(self):
        self.clone(100)
        types = {kind for (kind,) in rows(self.db, "SELECT DISTINCT Type FROM BaseItems WHERE Path LIKE ?", scale.CLONE_MARK)}
        self.assertEqual(types, {MOVIE, EPISODE})
        # three templates (Alpha, Beta, the episode); the folder and the owned extra are not repeated
        per_template = rows(self.db, "SELECT Name, COUNT(*) FROM BaseItems WHERE Path LIKE ? "
                            "GROUP BY substr(Name, 1, length(Name) - 6) ORDER BY 1", scale.CLONE_MARK)
        self.assertEqual({count for _, count in per_template}, {32, 31})
        self.assertEqual(rows(self.db, "SELECT COUNT(*) FROM BaseItems WHERE Name LIKE 'Trailer%'")[0][0], 1)  # the original only

    def test_every_clone_has_an_id_a_path_and_a_name_of_its_own(self):
        self.clone(100)
        for column in ("Id", "Path", "Name", "PresentationUniqueKey"):
            total, distinct = rows(self.db, f"SELECT COUNT({column}), COUNT(DISTINCT {column}) FROM BaseItems")[0]
            self.assertEqual(total, distinct, column)
        paths = [path for (path,) in rows(self.db, "SELECT Path FROM BaseItems WHERE Path LIKE ? ORDER BY Path", scale.CLONE_MARK)]
        self.assertEqual(paths[0], "/media/movies/scale-00001/Alpha.mkv")
        self.assertTrue(all("/scale-" in path for path in paths))
        # spelled as the template's id is: upper-case and dashed
        ids = [ident for (ident,) in rows(self.db, "SELECT Id FROM BaseItems WHERE Path LIKE ?", scale.CLONE_MARK)]
        self.assertTrue(all(ident == ident.upper() and len(ident) == 36 for ident in ids))
        self.assertEqual(rows(self.db, "SELECT Name, SortName, CleanName FROM BaseItems WHERE Path = ?",
                              "/media/movies/scale-00001/Alpha.mkv"), [("Alpha 00001", "Alpha 00001", "alpha 00001")])

    def test_rows_that_hang_off_a_template_follow_every_clone(self):
        self.clone(100)
        clones = 100 - 5
        # each of the three templates has two streams, one ancestor row, one image and one item value
        for table in ("MediaStreamInfos", "AncestorIds", "BaseItemImageInfos", "ItemValuesMap"):
            now = rows(self.db, f'SELECT COUNT(*) FROM "{table}" WHERE ItemId IN (SELECT Id FROM BaseItems WHERE Path LIKE ?)',
                       scale.CLONE_MARK)[0][0]
            per = 2 if table == "MediaStreamInfos" else 1
            self.assertEqual(now, clones * per, table)
        # an image row needs an id of its own; item values are shared, not copied
        images = rows(self.db, "SELECT COUNT(*), COUNT(DISTINCT Id) FROM BaseItemImageInfos")[0]
        self.assertEqual(images[0], images[1])
        self.assertEqual(rows(self.db, "SELECT COUNT(*) FROM ItemValues")[0][0], 1)

    def test_one_clone_in_userdata_every_carries_the_play_state_of_every_user(self):
        added = self.clone(100)
        marked = rows(self.db, "SELECT COUNT(DISTINCT ItemId) FROM UserData WHERE ItemId IN "
                      "(SELECT Id FROM BaseItems WHERE Path LIKE ?)", scale.CLONE_MARK)[0][0]
        self.assertEqual(marked, (100 - 5) // scale.USERDATA_EVERY)
        self.assertEqual(added["UserData"], marked * len(USERS))
        # a key of its own per clone item, shared by that item's rows of the different users
        keys = rows(self.db, "SELECT COUNT(*), COUNT(DISTINCT CustomDataKey) FROM UserData WHERE ItemId IN "
                    "(SELECT Id FROM BaseItems WHERE Path LIKE ?)", scale.CLONE_MARK)[0]
        self.assertEqual(keys, (marked * len(USERS), marked))

    def test_a_template_with_no_play_state_borrows_the_first_row_of_each_user(self):
        self.clone(100)
        # the episode has no UserData of its own; its clones still carry both users' state
        episode = rows(self.db, "SELECT Id FROM BaseItems WHERE Type = ? AND Path LIKE ? ORDER BY Path", EPISODE, scale.CLONE_MARK)
        withstate = [ident for (ident,) in episode if rows(self.db, "SELECT COUNT(*) FROM UserData WHERE ItemId = ?", ident)[0][0]]
        self.assertTrue(withstate)
        for ident in withstate:
            self.assertEqual(rows(self.db, "SELECT COUNT(DISTINCT UserId) FROM UserData WHERE ItemId = ?", ident)[0][0], len(USERS))

    def test_the_clones_add_no_foreign_key_violation(self):
        self.clone(100)
        self.assertEqual(rows(self.db, "SELECT COUNT(*) FROM pragma_foreign_key_check")[0][0], 0)

    def test_building_twice_writes_the_same_rows(self):
        other = Path(self.dir.name) / "again.db"
        library(other)
        self.clone(100)
        with closing(sqlite3.connect(other)) as conn:
            conn.execute("PRAGMA foreign_keys = OFF")
            with conn:
                scale.clone_items(conn, 100)
        sql = "SELECT Id, Path, Name FROM BaseItems ORDER BY Id"
        self.assertEqual(rows(self.db, sql), rows(other, sql))

    def test_a_table_that_gains_a_key_to_items_fails_the_build_instead_of_going_uncloned(self):
        with closing(sqlite3.connect(self.db)) as conn:
            conn.execute("CREATE TABLE Bookmarks (ItemId TEXT REFERENCES BaseItems(Id), Ticks INTEGER)")
            conn.commit()
        with self.assertRaisesRegex(CheckError, "does not clone the rows of Bookmarks"):
            self.clone(100)

    def test_a_library_already_that_large_or_with_nothing_to_repeat_is_refused(self):
        with self.assertRaisesRegex(CheckError, "already holds"):
            self.clone(5)
        other = Path(self.dir.name) / "empty.db"
        library(other)
        with closing(sqlite3.connect(other)) as conn:
            conn.execute("DELETE FROM BaseItems WHERE Type <> ?", (FOLDER,))
            conn.commit()
            with self.assertRaisesRegex(CheckError, "no movie, episode or song"):
                scale.clone_items(conn, 100)


class TemplateTest(unittest.TestCase):
    """Each exclusion of templates() is isolated by a row that only it excludes."""

    ZERO32 = "0" * 32

    def test_only_unowned_unversioned_non_extra_media_with_a_path_is_repeated(self):
        dir_ = tempfile.TemporaryDirectory()
        self.addCleanup(dir_.cleanup)
        db = Path(dir_.name) / "t.db"
        with closing(sqlite3.connect(db)) as conn:
            conn.executescript(SCHEMA)
            for ident, kind, path, owner, primary, extra in [
                    ("A1", MOVIE, "/m/a/a.mkv", None, None, None),             # a template: no owner at all
                    ("A2", EPISODE, "/m/b/b.mkv", ZERO, None, 0),              # a template: dashed zero Guid, ExtraType 0
                    ("A3", MOVIE, "/m/c/c.mkv", self.ZERO32, None, None),      # a template: undashed zero Guid
                    ("B1", FOLDER, "/m/d", None, None, None),                  # not media
                    ("B2", MOVIE, None, None, None, None),                     # no path
                    ("B3", MOVIE, "/m/e/e.mkv", "A1", None, None),             # owned by another item
                    ("B4", MOVIE, "/m/f/f.mkv", ZERO, "A1", None),             # a version of another item
                    ("B5", MOVIE, "/m/g/g.mkv", ZERO, None, 1)]:               # an extra
                conn.execute("INSERT INTO BaseItems (Id, Type, Path, OwnerId, PrimaryVersionId, ExtraType, Name) "
                             "VALUES (?,?,?,?,?,?,?)", (ident, kind, path, owner, primary, extra, ident))
            self.assertEqual([row["Id"] for row in scale.templates(conn)], ["A1", "A2", "A3"])


class BuildTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.root = Path(self.dir.name)
        source = self.root / scale.DERIVED[1]
        (source / "data").mkdir(parents=True)
        library(source / "data/jellyfin.db")
        (source / "synthetic-ready.json").write_text("{}")
        self.target = self.root / scale.DERIVED[0]

    def tearDown(self):
        self.dir.cleanup()

    def test_the_derived_fixture_is_a_copy_with_a_manifest_and_the_clean_fixture_is_untouched(self):
        scale.build(self.root, 100)
        self.assertEqual(rows(self.target / "data/jellyfin.db", "SELECT COUNT(*) FROM BaseItems")[0][0], 100)
        self.assertEqual(rows(self.root / scale.DERIVED[1] / "data/jellyfin.db", "SELECT COUNT(*) FROM BaseItems")[0][0], 5)
        manifest = json.loads((self.target / "scale.json").read_text())
        self.assertEqual((manifest["items"], manifest["source"], manifest["added"]["BaseItems"]), (100, "jellyfin-10.11.11", 95))
        self.assertEqual(os.stat(self.target / "scale.json").st_mode & 0o777, 0o600)

    def test_a_built_fixture_is_kept_and_an_unbuilt_source_is_named(self):
        scale.build(self.root, 100)
        before = (self.target / "scale.json").read_text()
        scale.build(self.root, 200)
        self.assertEqual((self.target / "scale.json").read_text(), before)
        (self.root / scale.DERIVED[1] / "synthetic-ready.json").unlink()
        (self.target / "scale.json").unlink()
        with self.assertRaisesRegex(CheckError, "is not built"):
            scale.build(self.root, 100)

    def test_a_directory_left_by_a_failed_build_is_not_silently_reused(self):
        self.target.mkdir()
        with self.assertRaisesRegex(CheckError, "is incomplete"):
            scale.build(self.root, 100)
        self.assertTrue(self.target.exists())  # not ours to delete

    def test_a_failed_build_removes_its_own_copy_and_leaves_the_clean_fixture(self):
        with patch.object(scale, "clone_items", side_effect=CheckError("boom")):
            with self.assertRaisesRegex(CheckError, "boom"):
                scale.build(self.root, 100)
        self.assertFalse(self.target.exists())
        self.assertTrue((self.root / scale.DERIVED[1] / "data/jellyfin.db").exists())


class SurvivalTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.db = Path(self.dir.name) / "jellyfin.db"
        library(self.db)
        with closing(sqlite3.connect(self.db)) as conn:
            conn.execute("PRAGMA foreign_keys = OFF")
            with conn:
                scale.clone_items(conn, 100)
        self.before = scale.counts(self.db)

    def tearDown(self):
        self.dir.cleanup()

    def test_counts_cover_the_clones_and_everything_that_hangs_off_them(self):
        self.assertEqual(self.before["BaseItems"], 95)
        self.assertEqual(self.before["MediaStreamInfos"], 190)
        self.assertGreater(self.before["UserData"], 0)

    def test_nothing_lost_is_nothing_reported_and_a_row_only_the_later_database_holds_is_fine(self):
        self.assertEqual(scale.compare(self.before, scale.counts(self.db)), [])
        with closing(sqlite3.connect(self.db)) as conn:
            conn.execute("INSERT INTO MediaStreamInfos SELECT ItemId, 9, 'extra' FROM MediaStreamInfos LIMIT 1")
            conn.commit()
        self.assertEqual(scale.compare(self.before, scale.counts(self.db)), [])

    def test_a_deleted_clone_and_a_deleted_child_row_are_each_named(self):
        with closing(sqlite3.connect(self.db)) as conn:
            conn.execute("DELETE FROM MediaStreamInfos WHERE ItemId IN (SELECT Id FROM BaseItems WHERE Path LIKE ? LIMIT 3)",
                         (scale.CLONE_MARK,))
            conn.execute("DELETE FROM BaseItems WHERE Path LIKE ? AND Name LIKE '%00010'", (scale.CLONE_MARK,))
            conn.commit()
        failures = scale.compare(self.before, scale.counts(self.db))
        self.assertIn("BaseItems: 95 rows before, 94 after", failures)
        self.assertTrue(any(line.startswith("MediaStreamInfos: 190 rows before") for line in failures), failures)

    def test_the_cli_baseline_and_check_round_trip(self):
        baseline = Path(self.dir.name) / "base.json"
        self.assertEqual(scale.main(["counts", str(self.db), str(baseline)]), 0)
        self.assertEqual(scale.main(["check", str(self.db), str(baseline)]), 0)
        with closing(sqlite3.connect(self.db)) as conn:
            conn.execute("DELETE FROM UserData")
            conn.commit()
        with patch("builtins.print") as printed:
            self.assertEqual(scale.main(["check", str(self.db), str(baseline)]), 1)
        self.assertIn("UserData", printed.call_args_list[0].args[0])

    def test_a_baseline_with_no_clone_is_refused_rather_than_trivially_passing(self):
        baseline = Path(self.dir.name) / "base.json"
        baseline.write_text(json.dumps({"counts": {"BaseItems": 0}}))
        with patch("builtins.print"):
            self.assertEqual(scale.main(["check", str(self.db), str(baseline)]), 1)


class DiskTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.fixture = Path(self.dir.name) / "fixture"
        (self.fixture / "data").mkdir(parents=True)
        (self.fixture / "data/jellyfin.db").write_bytes(os.urandom(1_000_000))
        (self.fixture / "data/log.txt").write_bytes(os.urandom(8192))

    def tearDown(self):
        self.dir.cleanup()

    def test_allocated_counts_what_the_files_occupy_not_links_or_gaps(self):
        os.symlink(self.fixture / "data/jellyfin.db", self.fixture / "link")
        self.assertGreaterEqual(scale.allocated(self.fixture), 1_000_000)
        self.assertLess(scale.allocated(self.fixture), 1_100_000)

    def test_enough_free_disk_is_none_and_too_little_says_how_much_and_why(self):
        base = os.stat(self.fixture / "data/jellyfin.db").st_size
        self.assertIsNone(scale.disk_shortfall(self.fixture, self.dir.name, free=lambda _: 10 * scale.FREE_MULTIPLE * base))
        reason = scale.disk_shortfall(self.fixture, self.dir.name, free=lambda _: base)
        self.assertIn("needs ", reason)
        self.assertIn(f"{scale.FREE_MULTIPLE}x the {scale.format_bytes(base)} database", reason)
        self.assertIn("has " + scale.format_bytes(base), reason)

    def test_the_wal_counts_as_database(self):
        (self.fixture / "data/jellyfin.db-wal").write_bytes(os.urandom(500_000))
        reason = scale.disk_shortfall(self.fixture, self.dir.name, free=lambda _: 1)
        self.assertIn(scale.format_bytes(1_500_000), reason)

    def test_free_space_of_a_directory_that_does_not_exist_yet_is_its_nearest_parent(self):
        self.assertGreater(scale.free_space_for(Path(self.dir.name) / "a/b/c"), 0)

    def test_sizes_read_in_the_unit_that_fits(self):
        self.assertEqual([scale.format_bytes(n) for n in (0, 1023, 1024, 1536, 5 * 1024 ** 2, 3 * 1024 ** 3, 5000 * 1024 ** 3)],
                         ["0 B", "1023 B", "1.0 KiB", "1.5 KiB", "5.0 MiB", "3.0 GiB", "5000.0 GiB"])

    def test_the_cli_refuses_with_the_reason_and_exit_one(self):
        with patch.object(scale, "free_space_for", return_value=1), patch("builtins.print") as printed:
            self.assertEqual(scale.main(["need-disk", str(self.fixture), self.dir.name]), 1)
        self.assertIn("needs", printed.call_args.args[0])
        with patch.object(scale, "free_space_for", return_value=10 ** 12), patch("builtins.print") as printed:
            self.assertEqual(scale.main(["need-disk", str(self.fixture), self.dir.name]), 0)
        printed.assert_not_called()


class SampleTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.data = Path(self.dir.name) / "config"
        (self.data / "data").mkdir(parents=True)
        self.peak = Path(self.dir.name) / "memory.peak"
        self.stat = Path(self.dir.name) / "memory.stat"
        self.stat.write_text("anon 100\nfile 5\n")

    def tearDown(self):
        self.dir.cleanup()

    def run_samples(self, steps, locate):
        """`steps` run once per sample (grow or shrink files); the sampler stops after they are used."""
        remaining = list(steps)

        def stop():
            return not remaining

        def sleep(_):
            remaining.pop(0)()

        return scale.sample("c", self.data, stop, 0, locate=locate, sleep=sleep)

    def test_disk_and_wal_report_their_largest_size_even_when_they_shrink_later(self):
        db = self.data / "data/jellyfin.db"
        wal = self.data / "data/jellyfin.db-wal"
        self.peak.write_text("42")
        result = self.run_samples([lambda: (db.write_bytes(os.urandom(2_000_000)), wal.write_bytes(os.urandom(900_000))),
                                   lambda: wal.unlink(), lambda: None], lambda c: self.peak)
        self.assertGreaterEqual(result["disk_bytes"], 2_900_000)
        self.assertGreaterEqual(result["wal_bytes"], 900_000)
        self.assertEqual(result["samples"], 4)  # one per step, and the last after the stop

    def test_a_wal_that_is_never_there_is_none_not_zero_and_never_a_crash(self):
        # SQLite removes the WAL when the migrations' connection closes, so a sample can land with none
        self.peak.write_text("42")
        result = self.run_samples([lambda: None, lambda: None], lambda c: self.peak)
        self.assertIsNone(result["wal_bytes"])
        self.assertEqual(result["samples"], 3)

    def test_memory_is_the_largest_peak_the_cgroup_reported_and_anonymous_the_largest_seen(self):
        self.peak.write_text("1000")
        result = self.run_samples([lambda: (self.peak.write_text("5000"), self.stat.write_text("anon 900\n")),
                                   lambda: (self.peak.write_text("3000"), self.stat.write_text("anon 400\n"))],
                                  lambda c: self.peak)
        self.assertEqual((result["memory_bytes"], result["anon_bytes"], result["memory_error"]), (5000, 900, None))

    def test_a_memory_stat_without_an_anon_line_leaves_anonymous_unmeasured(self):
        self.peak.write_text("1000")
        self.stat.write_text("file 5\n")
        result = self.run_samples([lambda: None], lambda c: self.peak)
        self.assertEqual(result["memory_bytes"], 1000)
        self.assertIsNone(result["anon_bytes"])
        self.assertRegex(result["anon_error"], "no anon line")
        self.assertIsNone(result["memory_error"])

    def test_memory_that_was_never_readable_says_why(self):
        def nowhere(_):
            raise CheckError("no cgroup")
        result = self.run_samples([lambda: None], nowhere)
        self.assertEqual((result["memory_bytes"], result["anon_bytes"], result["memory_error"]), (None, None, "no cgroup"))
        self.assertEqual(result["samples"], 2)

    def test_a_container_that_goes_away_keeps_the_peak_already_read(self):
        self.peak.write_text("7000")
        result = self.run_samples([self.peak.unlink], lambda c: self.peak)
        self.assertEqual(result["memory_bytes"], 7000)
        self.assertIsNone(result["memory_error"])

    def test_the_cgroup_is_found_from_the_container_pid(self):
        root, proc = Path(self.dir.name) / "cg", Path(self.dir.name) / "proc"
        (proc / "4242").mkdir(parents=True)
        (proc / "4242/cgroup").write_text("0::/system.slice/docker-abc.scope\n")
        (root / "system.slice/docker-abc.scope").mkdir(parents=True)
        (root / "system.slice/docker-abc.scope/memory.peak").write_text("9")
        done = type("Done", (), {"stdout": "4242\n"})()
        with patch.object(scale.subprocess, "run", return_value=done) as run:
            found = scale.locate_peak("adopt-x", root, proc)
        self.assertEqual(found, root / "system.slice/docker-abc.scope/memory.peak")
        self.assertEqual(run.call_args.args[0], ["docker", "inspect", "-f", "{{.State.Pid}}", "adopt-x"])

    def test_a_stopped_container_or_a_host_without_cgroup_v2_is_named_not_guessed(self):
        proc = Path(self.dir.name) / "proc"
        (proc / "0").mkdir(parents=True)
        (proc / "0/cgroup").write_text("")
        done = type("Done", (), {"stdout": "0\n"})()
        with patch.object(scale.subprocess, "run", return_value=done):
            with self.assertRaisesRegex(CheckError, "no cgroup v2 path"):
                scale.locate_peak("c", Path(self.dir.name), proc)
        (proc / "7").mkdir()
        (proc / "7/cgroup").write_text("12:memory:/docker/abc\n")
        with patch.object(scale.subprocess, "run", return_value=type("Done", (), {"stdout": "7\n"})()):
            with self.assertRaisesRegex(CheckError, "no cgroup v2 path"):
                scale.locate_peak("c", Path(self.dir.name), proc)


def log(*events):
    return "\n".join(json.dumps({"timestamp": stamp, "level": "INFO", "fields": {"message": message}})
                     for stamp, message in events)


class RecordTest(unittest.TestCase):
    START, DONE = "ferrofin-server starting", "database migrations applied"

    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.db = Path(self.dir.name) / "jellyfin.db"
        library(self.db)

    def tearDown(self):
        self.dir.cleanup()

    def test_migration_time_runs_from_the_server_start_to_the_applied_migrations(self):
        text = log(("2026-10-20T12:00:00.250000000Z", self.START), ("2026-10-20T12:00:05.000000Z", "something else"),
                   ("2026-10-20T12:01:30.750000Z", self.DONE), ("2026-10-20T12:09:00Z", self.DONE))
        self.assertEqual(scale.boot_seconds("not json\n" + text), 90.5)

    def test_a_log_missing_either_line_is_named(self):
        with self.assertRaisesRegex(CheckError, "both the server start and the applied migrations"):
            scale.boot_seconds(log(("2026-10-20T12:00:00Z", self.START)))

    def test_the_foreign_key_check_is_timed_on_the_file(self):
        self.assertGreaterEqual(scale.foreign_key_seconds(self.db), 0)

    def test_every_number_is_recorded_and_summarised(self):
        text = log(("2026-10-20T12:00:00Z", self.START), ("2026-10-20T12:00:42Z", self.DONE))
        samples = {"memory_bytes": 3 * 1024 ** 3, "anon_bytes": 2 * 1024 ** 3, "disk_bytes": 900 * 1024 ** 2,
                   "wal_bytes": 200 * 1024 ** 2, "samples": 40}
        result = scale.record(text, samples, self.db)
        self.assertEqual(result["missing"], [])
        self.assertEqual(result["migration_seconds"], 42)
        self.assertRegex(scale.summary(result), r"^migration 42\.0 s; peak memory 3\.0 GiB \(cgroup, page cache included; "
                         r"anonymous 2\.0 GiB\); disk high-water 900\.0 MiB \(WAL 200\.0 MiB\); foreign_key_check [0-9.]+ s$")

    def test_a_number_that_could_not_be_measured_is_named_and_fails_the_record(self):
        samples = {"memory_bytes": None, "memory_error": "no cgroup", "disk_bytes": 0, "wal_bytes": None}
        result = scale.record("", samples, self.db)
        self.assertEqual([line.split(":")[0] for line in result["missing"]], ["migration time", "peak memory", "disk high-water"])
        self.assertIn("peak memory: no cgroup", result["missing"])
        self.assertRegex(scale.summary(result), r"^migration not measured; peak memory not measured \(cgroup, page cache "
                         r"included; anonymous not measured\); disk high-water not measured \(WAL not measured\); "
                         r"foreign_key_check [0-9.]+ s$")
        logfile, saved, out = (Path(self.dir.name) / name for name in ("boot.log", "samples.json", "out.json"))
        logfile.write_text("")
        saved.write_text(json.dumps(samples))
        with patch("builtins.print") as printed:
            self.assertEqual(scale.main(["record", str(logfile), str(saved), str(self.db), str(out)]), 1)
        self.assertTrue(any("not measured: peak memory: no cgroup" in call.args[0] for call in printed.call_args_list))
        self.assertIsNone(json.loads(out.read_text())["memory_bytes"])

    def test_a_wal_that_no_sample_found_and_anonymous_memory_never_read_are_each_named(self):
        text = log(("2026-10-20T12:00:00Z", self.START), ("2026-10-20T12:00:42Z", self.DONE))
        samples = {"memory_bytes": 1, "anon_bytes": None, "disk_bytes": 1, "wal_bytes": None, "samples": 3}
        missing = scale.record(text, samples, self.db)["missing"]
        self.assertEqual(missing, ["anonymous memory: never sampled", "WAL peak: none of 3 samples found a WAL"])
        samples["anon_error"] = "memory.stat has no anon line"
        self.assertEqual(scale.record(text, samples, self.db)["missing"][0], "anonymous memory: memory.stat has no anon line")

    def test_a_zero_byte_wal_is_a_measurement_not_a_missing_number(self):
        text = log(("2026-10-20T12:00:00Z", self.START), ("2026-10-20T12:00:42Z", self.DONE))
        samples = {"memory_bytes": 1, "anon_bytes": 1, "disk_bytes": 1, "wal_bytes": 0, "samples": 3}
        result = scale.record(text, samples, self.db)
        self.assertEqual(result["missing"], [])
        self.assertIn("WAL 0 B", scale.summary(result))

    def test_a_sampler_that_left_no_file_is_a_missing_number_not_a_crash_or_a_stale_answer(self):
        text = Path(self.dir.name) / "boot.log"
        text.write_text(log(("2026-10-20T12:00:00Z", self.START), ("2026-10-20T12:00:42Z", self.DONE)))
        out = Path(self.dir.name) / "out.json"
        with patch("builtins.print") as printed:
            code = scale.main(["record", str(text), str(Path(self.dir.name) / "absent.json"), str(self.db), str(out)])
        self.assertEqual(code, 1)
        self.assertTrue(any("peak memory: the sampler left no samples" in call.args[0] for call in printed.call_args_list))


if __name__ == "__main__":
    unittest.main()
