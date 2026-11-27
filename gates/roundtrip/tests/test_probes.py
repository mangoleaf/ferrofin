"""probes.py: what it probes, read from a database shaped as Jellyfin's tables are, and how it judges answers."""

from contextlib import closing
import sqlite3
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.append(str(Path(__file__).resolve().parent.parent))
import probes  # noqa: E402


def scratch(test):
    """A directory removed when `test` ends."""
    made = tempfile.TemporaryDirectory()
    test.addCleanup(made.cleanup)
    return Path(made.name)

ITEM = "AF50F0EC-9D97-05C0-4D37-C27752B4B82B"
SHORT = "af50f0ec9d9705c04d37c27752b4b82b"


class Found(unittest.TestCase):
    def test_every_kind_is_read_from_its_table(self):
        db = scratch(self) / "jellyfin.db"
        with closing(sqlite3.connect(db)) as conn, conn:
            for statement in [
                'CREATE TABLE "BaseItemImageInfos" ("ItemId" TEXT, "ImageType" INTEGER)',
                f"INSERT INTO \"BaseItemImageInfos\" VALUES ('{ITEM}', 0), ('{ITEM}', 2), ('{ITEM}', 2), ('{ITEM}', 99)",
                'CREATE TABLE "Chapters" ("ItemId" TEXT, "ChapterIndex" INTEGER, "ImagePath" TEXT)',
                f"INSERT INTO \"Chapters\" VALUES ('{ITEM}', 1, '/config/x.jpg'), ('{ITEM}', 2, NULL)",
                'CREATE TABLE "ImageInfos" ("UserId" TEXT)',
                f"INSERT INTO \"ImageInfos\" VALUES ('{ITEM}')",
                'CREATE TABLE "TrickplayInfos" ("ItemId" TEXT, "Width" INTEGER)',
                f"INSERT INTO \"TrickplayInfos\" VALUES ('{ITEM}', 320)",
                'CREATE TABLE "MediaStreamInfos" ("ItemId" TEXT, "StreamIndex" INTEGER, "StreamType" INTEGER, '
                '"Codec" TEXT, "IsExternal" INTEGER)',
                f"INSERT INTO \"MediaStreamInfos\" VALUES ('{ITEM}', 3, 2, 'subrip', 1), ('{ITEM}', 4, 2, 'PGSSUB', 1), "
                f"('{ITEM}', 5, 2, 'ass', 0), ('{ITEM}', 0, 1, 'h264', 0)",
                'CREATE TABLE "AttachmentStreamInfos" ("ItemId" TEXT, "Index" INTEGER)',
                f"INSERT INTO \"AttachmentStreamInfos\" VALUES ('{ITEM}', 6)",
            ]:
                conn.execute(statement)
        self.assertEqual(probes.probes(db), [
            ("image", f"/Items/{SHORT}/Images/Primary/0", SHORT),
            ("image", f"/Items/{SHORT}/Images/Backdrop/0", SHORT),
            ("chapter", f"/Items/{SHORT}/Images/Chapter/1", SHORT),
            ("profile", f"/UserImage?userId={SHORT}", SHORT),
            ("trickplay", f"/Videos/{SHORT}/Trickplay/320/0.jpg", SHORT),
            ("subtitle", f"/Videos/{SHORT}/{SHORT}/Subtitles/3/Stream.srt", SHORT),
            ("attachment", f"/Videos/{SHORT}/{SHORT}/Attachments/6", SHORT),
        ])

    def test_a_database_without_the_tables_has_nothing_to_probe(self):
        db = scratch(self) / "jellyfin.db"
        sqlite3.connect(db).close()
        self.assertEqual(probes.probes(db), [])


class Server:
    def __init__(self, statuses):
        self.statuses = statuses

    def send(self, method, path):
        return self.statuses.get(path, 200), None


class Owners(unittest.TestCase):
    def test_every_item_and_user_in_the_route_form(self):
        db = scratch(self) / "jellyfin.db"
        with closing(sqlite3.connect(db)) as conn, conn:
            conn.execute('CREATE TABLE "BaseItems" ("Id" TEXT)')
            conn.execute('CREATE TABLE "Users" ("Id" TEXT)')
            conn.execute(f"INSERT INTO \"BaseItems\" VALUES ('{ITEM}')")
            conn.execute("INSERT INTO \"Users\" VALUES ('0F3B43C8-59FC-1020-8DCB-FAA582C4B920')")
            conn.execute('CREATE TABLE "BaseItemImageInfos" ("ItemId" TEXT, "ImageType" INTEGER)')
        self.assertEqual(probes.owners(db), {SHORT, "0f3b43c859fc10208dcbfaa582c4b920"})
        self.assertTrue(probes.has_images(db))
        empty = scratch(self) / "empty.db"
        sqlite3.connect(empty).close()
        self.assertFalse(probes.has_images(empty))


class Lost(unittest.TestCase):
    def test_a_file_no_longer_listed_for_an_item_still_there_is_named(self):
        before = [("trickplay", "/t1", "a"), ("trickplay", "/t2", "a"), ("image", "/i", "a"), ("image", "/j", "gone")]
        self.assertEqual(probes.lost([("trickplay", "/t1", "a"), ("image", "/i", "a")], before, {"a"}),
                         ["trickplay: 1 listed before the export and no longer, first: /t2"])
        self.assertEqual(probes.lost(before, before, {"a"}), [])


class Run(unittest.TestCase):
    def test_counts_each_kind_and_names_what_was_not_served(self):
        found = [("image", "/a", "x"), ("image", "/b", "x"), ("trickplay", "/t", "x")]
        problems, summary = probes.run(Server({"/b": 404}), found)
        self.assertEqual(summary, ["image 1/2", "trickplay 1/1"])
        self.assertEqual(problems, ["image: 1 of 2 not served, first: /b 404"])


if __name__ == "__main__":
    unittest.main()
