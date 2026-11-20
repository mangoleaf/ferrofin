"""checks_paths.py on databases shaped as Jellyfin's tables are, with an export tree beside them."""

from contextlib import closing
import sqlite3
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.append(str(Path(__file__).resolve().parent.parent))
import checks_paths  # noqa: E402


def scratch(test):
    """A directory removed when `test` ends."""
    made = tempfile.TemporaryDirectory()
    test.addCleanup(made.cleanup)
    return Path(made.name)

LIBRARY = checks_paths.LIBRARY


def database(path, statements):
    with closing(sqlite3.connect(path)) as conn, conn:
        for statement in statements:
            conn.execute(statement)
    return path


class Stored(unittest.TestCase):
    def setUp(self):
        self.dir = scratch(self)
        self.out, self.media = self.dir / "out", self.dir / "media"
        for made in ("out/root/default/Movies", "out/metadata/library/ab", "out/data/playlists/Mix",
                     "out/config/users/a", "media/movies"):
            (self.dir / made).mkdir(parents=True)
        (self.out / "metadata/library/ab/poster.jpg").write_bytes(b"x")
        (self.media / "movies/a.mkv").write_bytes(b"x")

    def check(self, *rows, sources=("/data",)):
        db = database(self.dir / "jellyfin.db", [
            'CREATE TABLE "BaseItems" ("Path" TEXT)',
            'CREATE TABLE "BaseItemImageInfos" ("Path" TEXT)',
            *(f"INSERT INTO \"{table}\" VALUES ('{path}')" for table, path in rows)])
        return checks_paths.stored(db, self.out, self.media, list(sources))

    def test_paths_in_the_layout_tokens_and_media_resolve(self):
        self.assertEqual(self.check(
            ("BaseItems", "/config/root/default/Movies"), ("BaseItems", "%AppDataPath%/playlists/Mix"),
            ("BaseItems", "/media/movies/a.mkv"), ("BaseItems", "http://tuner/stream"),
            ("BaseItemImageInfos", "%MetadataPath%/library/ab/poster.jpg"),
            ("BaseItemImageInfos", "/config/metadata/library/ab/poster.jpg")), [])

    def test_a_missing_file_and_a_path_under_the_source_are_named(self):
        problems = self.check(("BaseItems", "/config/root/default/Gone"), ("BaseItemImageInfos", "/data/metadata/x.jpg"))
        self.assertEqual(problems, [
            "BaseItems.Path: 1 of 1 path(s) not in the export, first: /config/root/default/Gone",
            "BaseItemImageInfos.Path: 1 path(s) under the source's directory, first: /data/metadata/x.jpg",
            "BaseItemImageInfos.Path: 1 of 1 path(s) not in the export, first: /data/metadata/x.jpg"])

    def test_an_absolute_path_outside_the_layout_never_resolves_whatever_the_host_holds(self):
        self.assertEqual(self.check(("BaseItems", str(self.media / "movies/a.mkv"))),
                         [f"BaseItems.Path: 1 of 1 path(s) not in the export, first: {self.media}/movies/a.mkv"])

    def test_a_directory_named_like_the_source_is_not_under_it(self):
        self.assertEqual(self.check(("BaseItems", "/database/x"), sources=("/data",)),
                         ["BaseItems.Path: 1 of 1 path(s) not in the export, first: /database/x"])

    def test_a_table_the_database_lacks_has_nothing_to_check(self):
        self.assertEqual(checks_paths.stored(database(self.dir / "empty.db", []), self.out, self.media, []), [])

    def test_an_items_table_without_a_path_is_not_a_pass(self):
        self.assertEqual(self.check(), ["BaseItems.Path: no stored path to check"])


class Libraries(unittest.TestCase):
    def setUp(self):
        self.dir = scratch(self)

    def db(self, name, *rows):
        return database(self.dir / name, [
            'CREATE TABLE "BaseItems" ("Id" TEXT, "Type" TEXT, "DateCreated" TEXT, "Name" TEXT)',
            *(f"INSERT INTO \"BaseItems\" VALUES ('{key}', '{LIBRARY}', '{created}', '{name}')"
              for key, created, name in rows)])

    def test_every_library_kept_with_its_date_in_either_form_passes(self):
        before = self.db("before.db", ("ab-1", "2026-11-04 07:46:44.5001116", "Movies"), ("cd-2", "2026-11-04 07:46:46", "Shows"))
        after = self.db("after.db", ("AB-1", "2026-11-04T07:46:44.5001116Z", "Movies"), ("CD-2", "2026-11-04 07:46:46.0000000", "Shows"))
        self.assertEqual(checks_paths.libraries(before, after), [])

    def test_no_library_before_the_export_is_not_a_pass(self):
        self.assertEqual(checks_paths.libraries(self.db("before.db"), self.db("after.db")),
                         ["no library folder row before the export: nothing to check"])

    def test_a_library_gone_or_recreated_is_named(self):
        before = self.db("before.db", ("ab-1", "2026-11-04 07:46:44", "Movies"), ("cd-2", "2026-11-04 07:46:46", "Shows"))
        after = self.db("after.db", ("AB-1", "2026-11-20 01:00:00", "Movies"))
        self.assertEqual(checks_paths.libraries(before, after), [
            "1 library folder row(s) gone after Jellyfin's scan: Shows",
            "1 library folder row(s) with another DateCreated: Movies (2026-11-04 07:46:44 -> 2026-11-20 01:00:00.0000000)"])


if __name__ == "__main__":
    unittest.main()
