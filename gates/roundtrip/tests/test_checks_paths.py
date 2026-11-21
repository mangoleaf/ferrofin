"""checks_paths.py on databases shaped as Jellyfin's tables are, with an export tree beside them."""

from contextlib import closing, redirect_stderr
import io
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

    def test_a_path_under_the_source_in_item_data_is_named(self):
        # a library's locations and a video's subtitle; keys, other paths and a document that is no JSON are not
        db = database(self.dir / "jellyfin.db", [
            'CREATE TABLE "BaseItems" ("Path" TEXT, "Data" TEXT)',
            """INSERT INTO "BaseItems" VALUES ('/config/root/default/Movies',
               '{"PhysicalLocationsList":["/data/root/default/Movies","/media/movies"],"/data/k":["/database/x"]}')""",
            """INSERT INTO "BaseItems" VALUES ('/media/movies/a.mkv',
               '{"SubtitleFiles":["/config/metadata/a.srt","/data/metadata/b.srt"],"IsHD":false}')""",
            """INSERT INTO "BaseItems" VALUES ('/media/movies/a.mkv', '/data/not json')"""])
        self.assertEqual(checks_paths.stored(db, self.out, self.media, ["/data"]), [
            "BaseItems.Data: 2 path(s) under the source's directory, first: "
            "/data/root/default/Movies, /data/metadata/b.srt"])

    def test_a_directory_named_like_the_source_is_not_under_it(self):
        self.assertEqual(self.check(("BaseItems", "/database/x"), sources=("/data",)),
                         ["BaseItems.Path: 1 of 1 path(s) not in the export, first: /database/x"])

    def source(self, *paths):
        """The database the export read, its server having run at /data on the copy `copy/`, with a person folder
        there and the playlists' data, and `paths` as BaseItems rows."""
        for made in ("copy/metadata/People/S/Synthetic Actor", "copy/data/playlists/Mix"):
            (self.dir / made).mkdir(parents=True)
        return database(self.dir / "before.db", ['CREATE TABLE "BaseItems" ("Path" TEXT)',
                                                 *(f"INSERT INTO \"BaseItems\" VALUES ('{path}')" for path in paths)])

    def test_a_path_the_source_stored_without_a_file_is_not_the_export_s_to_resolve(self):
        # a person whose folder Jellyfin never made, a file gone from the media, a folder under where Ferrofin ran
        before = self.source("%MetadataPath%/People/Z/Zoë", "/media/movies/gone.mkv", "/data/root/default/Gone")
        db = database(self.dir / "jellyfin.db", ['CREATE TABLE "BaseItems" ("Path" TEXT)', *(
            f"INSERT INTO \"BaseItems\" VALUES ('{path}')"
            for path in ("%MetadataPath%/People/Z/Zoë", "/media/movies/gone.mkv", "/config/root/default/Gone"))])
        self.assertEqual(checks_paths.stored(db, self.out, self.media, ["/data"], before,
                                             [("/data", str(self.dir / "copy"))]), [])

    def test_a_path_whose_file_the_source_held_still_resolves_in_the_export(self):
        # a person folder the export left behind, and a playlist folder rewritten to a wrong place
        before = self.source("%MetadataPath%/People/S/Synthetic Actor", "/data/data/playlists/Mix")
        db = database(self.dir / "jellyfin.db", ['CREATE TABLE "BaseItems" ("Path" TEXT)', *(
            f"INSERT INTO \"BaseItems\" VALUES ('{path}')"
            for path in ("%MetadataPath%/People/S/Synthetic Actor", "/config/dat/playlists/Mix"))])
        self.assertEqual(checks_paths.stored(db, self.out, self.media, ["/data"], before,
                                             [("/data", str(self.dir / "copy"))]),
                         ["BaseItems.Path: 2 of 2 path(s) not in the export, first: "
                          "%MetadataPath%/People/S/Synthetic Actor, /config/dat/playlists/Mix"])

    def test_an_unrewritten_path_outside_every_directory_is_never_left_out(self):
        # a stale directory the export should have rewritten: the source stored it too, but nowhere it can be placed;
        # and a path left out of one column is not left out of another
        before = self.source("/moved/root/default/Old", str(self.media / "movies/a.mkv"), "%MetadataPath%/People/Z/Zoë")
        db = database(self.dir / "jellyfin.db", ['CREATE TABLE "BaseItems" ("Path" TEXT)',
                                                 'CREATE TABLE "BaseItemImageInfos" ("Path" TEXT)', *(
            f"INSERT INTO \"{table}\" VALUES ('{path}')"
            for table, path in (("BaseItems", "/moved/root/default/Old"), ("BaseItems", f"{self.media}/movies/a.mkv"),
                                ("BaseItemImageInfos", "%MetadataPath%/People/Z/Zoë")))])
        self.assertEqual(checks_paths.stored(db, self.out, self.media, ["/data"], before,
                                             [("/data", str(self.dir / "copy"))]),
                         [f"BaseItems.Path: 2 of 2 path(s) not in the export, first: /moved/root/default/Old, "
                          f"{self.media}/movies/a.mkv",
                          "BaseItemImageInfos.Path: 1 of 1 path(s) not in the export, first: %MetadataPath%/People/Z/Zoë"])

    def test_a_configuration_path_maps_back_to_the_configuration_directory(self):
        (self.dir / "etc").mkdir()
        ran = [("/var/lib/ferrofin", str(self.dir / "copy")), ("/etc/ferrofin", str(self.dir / "etc"))]
        self.assertEqual(checks_paths.source_forms("/config/config/users/x", ran),
                         {"/config/config/users/x", "/var/lib/ferrofin/config/users/x", "/etc/ferrofin/users/x"})

    def test_before_needs_the_directories_that_place_the_source_s_paths(self):
        with self.assertRaises(SystemExit) as refused, redirect_stderr(io.StringIO()) as err:
            checks_paths.main(["stored", "--db", "x", "--out", "o", "--media", "m", "--before", "b"])
        self.assertEqual(refused.exception.code, 2)
        self.assertIn("--before needs --ran", err.getvalue())

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
