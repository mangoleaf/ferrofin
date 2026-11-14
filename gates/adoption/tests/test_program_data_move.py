"""The program-data move checks (program_data_move.py) on invented rows and a local HTTP fixture."""

from contextlib import closing, redirect_stdout
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import io
import json
import os
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import unittest
import uuid

sys.path.insert(0, str(Path(__file__).parents[1]))
import program_data_move as move
import table_digests as digests
from metadata import CheckError
from test_metadata import png

OLD, NEW = "/var/lib/jellyfin", "/var/lib/ferrofin"
OLD_ETC, NEW_ETC = "/etc/jellyfin", "/etc/ferrofin"
FOLDER = "MediaBrowser.Controller.Entities.CollectionFolder"
LIBRARY = "AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA"
SHOWS = "BBBBBBBB-BBBB-BBBB-BBBB-BBBBBBBBBBBB"
ADMIN = "DDDDDDDD-DDDD-DDDD-DDDD-DDDDDDDDDDDD"
MOVIE = "CCCCCCCC-CCCC-CCCC-CCCC-CCCCCCCCCCCC"


def hexid(guid):
    return guid.replace("-", "").lower()


class Server(BaseHTTPRequestHandler):
    """The routes check_api requests: the libraries, and every image as a PNG unless it is missing."""
    folders = []
    missing = set()

    def do_GET(self):
        if self.path == "/Library/VirtualFolders":
            body, kind, status = json.dumps(self.folders).encode(), "application/json", 200
        elif self.path in self.missing:
            body, kind, status = b"", "text/plain", 404
        elif self.path.endswith("/Images/Primary"):
            body, kind, status = png(), "image/png", 200
        else:
            body, kind, status = b"", "text/plain", 404
        self.send_response(status)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


class ProgramDataMoveTest(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        # The copies on this host: the data directory holds the database, the configuration is apart.
        self.data, self.etc = root / "var-lib-ferrofin", root / "etc-ferrofin"
        self.moves = [(OLD, NEW, str(self.data)), (OLD_ETC, NEW_ETC, str(self.etc))]
        self.db = self.data / "data/jellyfin.db"
        self.db.parent.mkdir(parents=True)
        for path in (self.data / "root/default/Movies", self.data / "root/default/Shows", self.data / "metadata/library/aa/aa",
                     self.etc / "users/admin"):
            path.mkdir(parents=True)
        for path in (self.data / "metadata/library/aa/aa/poster.png", self.data / "metadata/library/aa/aa/chapter.jpg",
                     self.etc / "users/admin/profile.png"):
            path.write_bytes(b"x")
        with closing(sqlite3.connect(self.db)) as conn, conn:
            conn.executescript(f'''
                CREATE TABLE BaseItems (Id TEXT PRIMARY KEY, Type TEXT, Name TEXT, Path TEXT, Data TEXT);
                CREATE TABLE BaseItemImageInfos (Id TEXT PRIMARY KEY, ItemId TEXT, Path TEXT, ImageType INTEGER);
                CREATE TABLE Chapters (ItemId TEXT, ChapterIndex INTEGER, ImagePath TEXT, PRIMARY KEY (ItemId, ChapterIndex));
                CREATE TABLE MediaStreamInfos (ItemId TEXT, StreamIndex INTEGER, Path TEXT, PRIMARY KEY (ItemId, StreamIndex));
                CREATE TABLE ImageInfos (Id INTEGER PRIMARY KEY, Path TEXT, UserId TEXT);
                CREATE TABLE ApiKeys (AccessToken TEXT, DateCreated TEXT);
                CREATE TABLE Users (Id TEXT);
                CREATE TABLE Permissions (UserId TEXT, Kind INTEGER, Value INTEGER);
                INSERT INTO Users VALUES ('{ADMIN}');
                INSERT INTO Permissions VALUES ('{ADMIN}', 0, 1);
                INSERT INTO ApiKeys VALUES ('token', '2026-01-01');
                INSERT INTO BaseItems VALUES ('{LIBRARY}', '{FOLDER}', 'Movies', '{OLD}/root/default/Movies',
                  '{{"PhysicalLocationsList":["{OLD}/root/default/Movies","/media/movies"]}}');
                INSERT INTO BaseItems VALUES ('{SHOWS}', '{FOLDER}', 'Shows', '{OLD}/root/default/Shows', NULL);
                INSERT INTO BaseItems VALUES ('{MOVIE}', 'MediaBrowser.Controller.Entities.Movies.Movie', 'Film',
                  '/media/movies/Film.mkv', NULL);
                INSERT INTO BaseItemImageInfos VALUES ('11111111-1111-1111-1111-111111111111', '{LIBRARY}',
                  '{OLD}/metadata/library/aa/aa/poster.png', 0);
                -- a file the copy does not hold: its row must stay as it is
                INSERT INTO BaseItemImageInfos VALUES ('22222222-2222-2222-2222-222222222222', '{MOVIE}',
                  '{OLD}/metadata/library/aa/aa/gone.png', 2);
                INSERT INTO BaseItemImageInfos VALUES ('33333333-3333-3333-3333-333333333333', '{MOVIE}',
                  '/media/movies/poster.jpg', 0);
                INSERT INTO Chapters VALUES ('{MOVIE}', 0, '{OLD}/metadata/library/aa/aa/chapter.jpg');
                INSERT INTO MediaStreamInfos VALUES ('{MOVIE}', 2, '/media/movies/Film.en.srt');
                INSERT INTO ImageInfos VALUES (1, '{OLD_ETC}/users/admin/profile.png', '{ADMIN}');
            ''')
        self.baseline = move.snapshot(self.db, self.moves)
        Server.folders = [{"Name": "Movies", "ItemId": hexid(LIBRARY), "Locations": ["/media/movies"]},
                          {"Name": "Shows", "ItemId": hexid(SHOWS), "Locations": ["/media/shows"]}]
        Server.missing = set()
        self.http = ThreadingHTTPServer(("127.0.0.1", 0), Server)
        threading.Thread(target=self.http.serve_forever, daemon=True).start()
        self.addCleanup(self.http.server_close)
        self.addCleanup(self.http.shutdown)
        self.url = f"http://127.0.0.1:{self.http.server_port}"
        self.record = str(self.data / "snapshots.json")

    def sql(self, statement, *args):
        with closing(sqlite3.connect(self.db)) as conn, conn:
            conn.execute(statement, args)

    def repair(self):
        """What the boot repair does: each path whose file the copy holds, onto the copy; a snapshot first."""
        (self.db.parent / "jellyfin.db.pre-program-data-move").write_bytes(b"db")
        for table, column in move.COLUMNS:
            for _, (before, after) in self.baseline["paths"].get(table, {}).items():
                self.sql(f'UPDATE "{table}" SET "{column}" = ? WHERE "{column}" = ?', after, before)
        self.sql('UPDATE BaseItems SET Data = replace(Data, ?, ?)', OLD + "/", NEW + "/")

    def check(self):
        return move.check(self.db, json.loads(json.dumps(self.baseline)), self.url, self.record)

    def test_each_path_is_expected_at_its_place_in_the_copy_or_unchanged_when_the_copy_lacks_it(self):
        paths = self.baseline["paths"]
        self.assertEqual(paths["BaseItems"], {hexid(LIBRARY): [f"{OLD}/root/default/Movies", f"{NEW}/root/default/Movies"],
                                              hexid(SHOWS): [f"{OLD}/root/default/Shows", f"{NEW}/root/default/Shows"]})
        self.assertEqual(paths["BaseItemImageInfos"], {
            hexid("11111111-1111-1111-1111-111111111111"): [f"{OLD}/metadata/library/aa/aa/poster.png",
                                                             f"{NEW}/metadata/library/aa/aa/poster.png"],
            hexid("22222222-2222-2222-2222-222222222222"): [f"{OLD}/metadata/library/aa/aa/gone.png"] * 2})
        self.assertEqual(paths["Chapters"], {f"{hexid(MOVIE)}/0": [f"{OLD}/metadata/library/aa/aa/chapter.jpg",
                                                                   f"{NEW}/metadata/library/aa/aa/chapter.jpg"]})
        self.assertEqual(paths["MediaStreamInfos"], {})  # a subtitle beside the media did not move
        self.assertEqual(paths["ImageInfos"], {"1": [f"{OLD_ETC}/users/admin/profile.png",
                                                     f"{NEW_ETC}/users/admin/profile.png"]})
        self.assertEqual(self.baseline["libraries"], {hexid(LIBRARY): "Movies", hexid(SHOWS): "Shows"})
        self.assertEqual(self.baseline["library_images"], [hexid(LIBRARY)])
        self.assertEqual(self.baseline["profiles"], [hexid(ADMIN)])

    def test_a_directory_matches_whole_components_and_the_longest_wins(self):
        (self.data / "x").write_bytes(b"x")
        nested = [("/srv", "/new", str(self.etc)), ("/srv/data", "/srv2", str(self.data))]
        self.assertEqual(move.moved_path("/srv/data/x", nested), "/srv2/x")
        self.assertEqual(move.moved_path("/srv/database/x", nested), "/srv/database/x")
        self.assertEqual(move.moved_path(f"{OLD}2/root", self.moves), f"{OLD}2/root")
        self.assertEqual(move.moved_path(None, self.moves), None)

    def test_a_fixture_that_moves_nothing_is_refused(self):
        # A fixture Jellyfin wrote in its image's /config names no previous directory.
        with self.assertRaisesRegex(CheckError, "moves no path in BaseItems, BaseItemImageInfos, ImageInfos"):
            move.snapshot(self.db, [("/config", NEW, str(self.data))])
        # Nor does a copy that holds none of the files.
        with self.assertRaisesRegex(CheckError, "moves no path in BaseItems"):
            move.snapshot(self.db, [(OLD, NEW, str(self.data / "empty")), (OLD_ETC, NEW_ETC, str(self.etc))])
        with self.assertRaisesRegex(CheckError, "--moved"):
            move.snapshot(self.db, [])

    def test_a_repaired_database_passes_and_a_later_boot_that_changes_nothing_passes_too(self):
        self.repair()
        self.assertEqual(self.check(), [])
        self.assertTrue(Path(self.record).exists())
        self.assertEqual(self.check(), [])

    def test_a_start_that_rewrote_nothing_fails_on_every_moved_column_and_the_missing_snapshot(self):
        # The #1165 class: the paths stay where Jellyfin left them.
        failures = self.check()
        for table in ("BaseItems.Path: 2", "BaseItemImageInfos.Path: 1", "Chapters.ImagePath: 1", "ImageInfos.Path: 1"):
            self.assertTrue(any(line.startswith(table + " path(s) still name a previous directory") for line in failures),
                            (table, failures))
        self.assertIn("CollectionFolder Data still names a previous directory: 1 library(ies), first Movies", failures)
        self.assertIn("no jellyfin.db.pre-program-data-move beside the database: "
                      "the paths were not rewritten after a snapshot", failures)

    def test_a_path_rewritten_though_the_copy_lacks_its_file_fails(self):
        self.repair()
        self.sql("UPDATE BaseItemImageInfos SET Path = ? WHERE Path LIKE '%gone.png'", f"{NEW}/metadata/library/aa/aa/gone.png")
        self.assertEqual([line for line in self.check() if "BaseItemImageInfos" in line],
                         ["BaseItemImageInfos.Path: 1 path(s) read neither their place in the copy nor what they were; "
                          f"first 22222222222222222222222222222222: {NEW}/metadata/library/aa/aa/gone.png"])

    def test_a_row_written_since_under_a_previous_directory_fails_and_a_lost_one_too(self):
        self.repair()
        self.sql("INSERT INTO Chapters VALUES (?, 1, ?)", MOVIE, f"{OLD}/metadata/library/aa/aa/chapter.jpg")
        self.sql("DELETE FROM ImageInfos")
        failures = self.check()
        self.assertIn(f"Chapters.ImagePath: 1 path(s) were written since under a previous directory; first {hexid(MOVIE)}/1: "
                      f"{OLD}/metadata/library/aa/aa/chapter.jpg", failures)
        self.assertIn(f"ImageInfos.Path: 1 path(s) are gone; first 1: {OLD_ETC}/users/admin/profile.png", failures)

    def redraw(self, item, kind, path, file=True, gone=("11111111-1111-1111-1111-111111111111",)):
        """What the scan's library-tile pass does once a tile is 7 days old: the `gone` image rows
        replaced by one at a new id, here for `item` and `kind` at `path`, its file in the copy when
        `file`."""
        for key in gone:
            self.sql("DELETE FROM BaseItemImageInfos WHERE Id = ?", key)
        self.sql("INSERT INTO BaseItemImageInfos VALUES (?, ?, ?, ?)", str(uuid.uuid4()).upper(), item, path, kind)
        for _, new, host in self.moves:
            if file and path.startswith(new + "/"):
                copy = Path(host, path[len(new) + 1:])
                copy.parent.mkdir(parents=True, exist_ok=True)
                copy.write_bytes(b"x")

    def test_an_image_redrawn_since_at_a_new_id_in_the_copy_passes(self):
        self.repair()
        self.assertEqual(self.check(), [])
        self.redraw(LIBRARY, 0, f"{NEW}/metadata/library/{LIBRARY}/primary.png")
        self.assertEqual(self.check(), [])
        self.assertEqual(self.check(), [])
        # A baseline taken before the image rows' items and types were recorded is refused.
        with self.assertRaisesRegex(CheckError, "unsupported program-data move baseline"):
            move.validate({**self.baseline, "version": 1})

    def test_an_image_row_gone_without_a_redraw_of_its_item_and_type_in_the_copy_fails(self):
        tile = f"{NEW}/metadata/library/{LIBRARY}/primary.png"
        gone = (f"BaseItemImageInfos.Path: 1 path(s) are gone; first 11111111111111111111111111111111: "
                f"{OLD}/metadata/library/aa/aa/poster.png")
        for name, redraw in (("its file missing", (LIBRARY, 0, tile, False)),
                             ("another image type", (LIBRARY, 2, tile, True)),
                             ("another item", (SHOWS, 0, tile, True)),
                             ("outside the copy", (LIBRARY, 0, f"/media/movies/{LIBRARY}.png", True)),
                             ("under a previous directory", (LIBRARY, 0, f"{OLD}/metadata/library/aa/aa/chapter.jpg", True))):
            with self.subTest(name):
                self.setUp()
                self.repair()
                self.redraw(*redraw)
                self.assertIn(gone, self.check())
        self.setUp()
        self.repair()
        self.sql("DELETE FROM BaseItemImageInfos WHERE Id = '11111111-1111-1111-1111-111111111111'")
        self.assertEqual([line for line in self.check() if "BaseItemImageInfos" in line], [gone])

    def test_a_redraw_must_replace_every_image_of_the_type_that_went(self):
        for index in (0, 1):
            (self.data / f"metadata/library/aa/aa/backdrop{index}.jpg").write_bytes(b"x")
            self.sql("INSERT INTO BaseItemImageInfos VALUES (?, ?, ?, 2)", f"4444444{index}-4444-4444-4444-444444444444",
                     LIBRARY, f"{OLD}/metadata/library/aa/aa/backdrop{index}.jpg")
        self.baseline = move.snapshot(self.db, self.moves)
        self.repair()
        lines = lambda: [line for line in self.check() if "BaseItemImageInfos" in line]
        # A backdrop lost: the other, still at its recorded id, does not stand in for it.
        self.sql("DELETE FROM BaseItemImageInfos WHERE Id = '44444441-4444-4444-4444-444444444444'")
        self.assertEqual(lines(), [f"BaseItemImageInfos.Path: 1 path(s) are gone; first 44444441444444444444444444444444: "
                                   f"{OLD}/metadata/library/aa/aa/backdrop1.jpg"])
        # Both gone, one saved again at a new id: still one short, so both are reported.
        self.redraw(LIBRARY, 2, f"{NEW}/metadata/library/aa/aa/backdrop0.jpg", gone=("44444440-4444-4444-4444-444444444444",))
        self.assertEqual(lines(), [f"BaseItemImageInfos.Path: 2 path(s) are gone; first 44444440444444444444444444444444: "
                                   f"{OLD}/metadata/library/aa/aa/backdrop0.jpg"])
        self.redraw(LIBRARY, 2, f"{NEW}/metadata/library/aa/aa/backdrop1.jpg", gone=())
        self.assertEqual(self.check(), [])

    def test_a_library_added_or_lost_fails(self):
        self.repair()
        self.sql("INSERT INTO BaseItems VALUES ('EEEEEEEE-EEEE-EEEE-EEEE-EEEEEEEEEEEE', ?, 'Movies', ?, NULL)",
                 FOLDER, f"{NEW}/root/default/Movies")
        self.sql("DELETE FROM BaseItems WHERE Id = ?", SHOWS)
        self.assertIn("CollectionFolder rows changed: 1 added, 1 lost (of 2)", self.check())

    def test_virtual_folders_must_list_every_library_by_its_row_id_and_no_previous_location(self):
        self.repair()
        Server.folders = [{"Name": "Movies", "ItemId": "f" * 32, "Locations": [f"{OLD}/root/default/Movies"]}]
        failures = self.check()
        self.assertIn("/Library/VirtualFolders: 1 listed with another id than their CollectionFolder row, 1 not listed; "
                      "first Movies", failures)
        self.assertIn("/Library/VirtualFolders: 1 library(ies) list a location under a previous directory, first Movies",
                      failures)

    def test_artwork_that_is_not_served_fails(self):
        self.repair()
        Server.missing = {f"/Items/{hexid(LIBRARY)}/Images/Primary", f"/Users/{hexid(ADMIN)}/Images/Primary"}
        failures = self.check()
        self.assertIn(f"1 of 1 library Primary image(s) not served, first /Items/{hexid(LIBRARY)}/Images/Primary "
                      "(metadata HTTP request returned 404)", failures)
        self.assertIn(f"1 of 1 profile image(s) not served, first /Users/{hexid(ADMIN)}/Images/Primary "
                      "(metadata HTTP request returned 404)", failures)

    def test_a_later_boot_that_takes_or_rewrites_a_snapshot_fails(self):
        self.repair()
        self.assertEqual(self.check(), [])
        (self.db.parent / "jellyfin.db.pre-program-data-move").write_bytes(b"again")
        (self.db.parent / "jellyfin.db.pre-0050").write_bytes(b"db")
        (self.db.parent / "jellyfin.db.pre-0051.partial").write_bytes(b"db")  # not a snapshot yet
        failures = self.check()
        self.assertIn("a later boot took 1 database snapshot(s), first jellyfin.db.pre-0050", failures)
        self.assertIn("1 database snapshot(s) rewritten since the first boot, first jellyfin.db.pre-program-data-move",
                      failures)
        self.assertEqual(len(failures), 2)

    def test_the_command_line_writes_a_private_baseline_and_exits_with_the_verdict(self):
        baseline = self.data / "move.json"
        moved = [arg for old, new, host in self.moves for arg in ("--moved", old, new, host)]
        with redirect_stdout(io.StringIO()):
            self.assertEqual(move.main(["snapshot", str(self.db), str(baseline), *moved]), 0)
        self.assertEqual(baseline.stat().st_mode & 0o777, 0o600)
        self.assertEqual(json.loads(baseline.read_text()), json.loads(json.dumps(self.baseline)))
        out = io.StringIO()
        with redirect_stdout(out):
            self.assertEqual(move.main(["check", str(self.db), str(baseline), "--base-url", self.url]), 1)
        self.assertIn("still name a previous directory", out.getvalue())
        self.repair()
        with redirect_stdout(io.StringIO()):
            self.assertEqual(move.main(["check", str(self.db), str(baseline), "--base-url", self.url,
                                        "--snapshots", self.record]), 0)
        out = io.StringIO()
        with redirect_stdout(out):
            self.assertEqual(move.main(["check", str(self.db), str(baseline)]), 1)
        self.assertIn("--base-url is required", out.getvalue())
        out = io.StringIO()
        with redirect_stdout(out):
            self.assertEqual(move.main(["check", str(self.db), os.devnull, "--base-url", self.url]), 1)
        self.assertIn("could not read", out.getvalue())

    def test_the_table_digests_expect_the_moved_paths_at_their_new_place(self):
        # Chapters is the digested table with a path under program data: its baseline records the
        # chapter image where the repair must leave it, so a start that rewrote it passes and one
        # that did not fails.
        conn = sqlite3.connect(f"file:{self.db}?mode=ro", uri=True)
        conn.row_factory = sqlite3.Row
        with closing(conn):
            before = digests.table_rows(conn, "Chapters", moves=self.moves)[1]
            unmoved = digests.table_rows(conn, "Chapters")[1]
        self.repair()
        conn = sqlite3.connect(f"file:{self.db}?mode=ro", uri=True)
        conn.row_factory = sqlite3.Row
        with closing(conn):
            after = digests.table_rows(conn, "Chapters")[1]
        self.assertEqual(before, after)
        self.assertNotEqual(unmoved, after)


if __name__ == "__main__":
    unittest.main()
