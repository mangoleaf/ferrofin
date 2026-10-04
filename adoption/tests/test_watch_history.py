"""Watch-history preservation checks using invented items and users only."""

import contextlib
import copy
import io
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest
from urllib.parse import parse_qs, urlsplit
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parents[1]))
import watch_history as history

MOVIE = "a" * 32
EPISODE = "b" * 32
USER = "c" * 32
OTHER_USER = "d" * 32


class FakeApi:
    def __init__(self, expected):
        self.items = copy.deepcopy(expected["items"])
        self.hide_list = False
        self.calls = []

    def request(self, path, **kwargs):
        self.calls.append(path)
        if path.startswith("/Items?"):
            query = parse_qs(urlsplit(path).query)
            user = query["UserId"][0]
            ids = query["Ids"][0].split(",")
            return {"Items": [] if self.hide_list else [
                {"Id": item, "UserData": self.items[user + ":" + item]}
                for item in ids if user + ":" + item in self.items]}
        _, _, user, _, item = path.split("/")
        key = user + ":" + item
        return {"Id": item, "UserData": self.items[key]} if key in self.items else None


class WatchHistoryTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.db = Path(self.tmp.name) / "fixture.db"
        self.baseline = Path(self.tmp.name) / "history.json"
        with contextlib.closing(sqlite3.connect(self.db)) as db, db:
            db.executescript('''
                CREATE TABLE BaseItems(Id TEXT, Type TEXT);
                CREATE TABLE UserData(ItemId TEXT, UserId TEXT, CustomDataKey TEXT,
                    Played INTEGER, PlayCount INTEGER, PlaybackPositionTicks INTEGER,
                    LastPlayedDate TEXT, IsFavorite INTEGER, SubtitleStreamIndex INTEGER);
            ''')
            for item, kind in [(MOVIE, "Movie"), (EPISODE, "Episode")]:
                db.execute("INSERT INTO BaseItems VALUES (?,?)", (item, kind))
                for user in (USER, OTHER_USER):
                    db.execute("INSERT INTO UserData VALUES (?,?,?,?,?,?,?,?,?)", (
                        item, user, str(history.uuid.UUID(item)), int(user == USER),
                        2, 123456789, "2026-01-01 12:00:00", 1, 2))
            db.execute("INSERT INTO UserData SELECT ItemId,UserId,'private-provider-id',"
                       "Played,PlayCount,PlaybackPositionTicks,LastPlayedDate,IsFavorite,"
                       "SubtitleStreamIndex FROM UserData LIMIT 1")
        self.expected = history.snapshot(self.db)

    def sql(self, statement):
        with contextlib.closing(sqlite3.connect(self.db)) as db, db:
            db.executescript(statement)

    def test_unchanged_database_and_api_pass(self):
        self.assertFalse(history.compare_db(self.expected, history.snapshot(self.db)))
        self.assertFalse(history.compare_api(self.expected, FakeApi(self.expected)))

    def test_every_watch_field_change_is_detected_in_database(self):
        for field in history.FIELDS:
            with self.subTest(field=field):
                with contextlib.closing(sqlite3.connect(self.db)) as db:
                    original = db.execute("SELECT " + field + ",rowid FROM UserData").fetchall()
                self.sql("UPDATE UserData SET " + field + " = NULL")
                self.assertTrue(history.compare_db(self.expected, history.snapshot(self.db)))
                with contextlib.closing(sqlite3.connect(self.db)) as db, db:
                    db.executemany("UPDATE UserData SET " + field + "=? WHERE rowid=?", original)

    def test_every_watch_field_change_is_detected_in_api(self):
        for field in history.FIELDS:
            with self.subTest(field=field):
                api = FakeApi(self.expected)
                api.items[USER + ":" + MOVIE][field] = None
                self.assertEqual(history.compare_api(self.expected, api),
                                 {f"API changed {field}": 1})

    def test_deleted_provider_row_fails_even_when_guid_row_survives(self):
        self.sql("DELETE FROM UserData WHERE CustomDataKey='private-provider-id'")
        self.assertEqual(history.compare_db(self.expected, history.snapshot(self.db)),
                         {"database missing watch-history row": 1})

    def test_progress_loss_for_another_user_is_detected(self):
        api = FakeApi(self.expected)
        api.items[OTHER_USER + ":" + EPISODE]["PlaybackPositionTicks"] = 0
        self.assertEqual(history.compare_api(self.expected, api),
                         {"API changed PlaybackPositionTicks": 1})

    def test_unwatched_items_cannot_become_watched(self):
        api = FakeApi(self.expected)
        api.items[OTHER_USER + ":" + MOVIE]["Played"] = True
        self.assertEqual(history.compare_api(self.expected, api), {"API changed Played": 1})

    def test_missing_user_data_or_item_fails(self):
        for missing in (None, "deleted"):
            with self.subTest(missing=missing):
                api = FakeApi(self.expected)
                if missing is None:
                    api.items[USER + ":" + MOVIE] = None
                else:
                    del api.items[USER + ":" + MOVIE]
                self.assertEqual(history.compare_api(self.expected, api),
                                 {"API missing watch history": 1})

    def test_hidden_versions_are_checked_through_detail_routes(self):
        api = FakeApi(self.expected)
        api.hide_list = True
        self.assertFalse(history.compare_api(self.expected, api))
        self.assertEqual(sum(path.startswith("/Users/") for path in api.calls), 4)

    def test_date_formats_and_guid_casing_do_not_cause_false_losses(self):
        self.sql("UPDATE UserData SET ItemId=upper(ItemId), UserId=upper(UserId)")
        self.assertFalse(history.compare_db(self.expected, history.snapshot(self.db)))
        api = FakeApi(self.expected)
        for row in api.items.values():
            row["LastPlayedDate"] = "2026-01-01T12:00:00.0000000Z"
        self.assertFalse(history.compare_api(self.expected, api))

    def test_baseline_is_private_and_contains_no_provider_keys(self):
        self.assertEqual(history.main(["snapshot", str(self.db), str(self.baseline)]), 0)
        self.assertEqual(self.baseline.stat().st_mode & 0o777, 0o600)
        self.assertNotIn("private-provider-id", self.baseline.read_text())

    def test_fixture_without_watched_history_fails(self):
        self.sql("UPDATE UserData SET Played=0")
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(history.main(["snapshot", str(self.db), str(self.baseline)]), 1)

    def test_fixture_without_resume_progress_fails(self):
        self.sql("UPDATE UserData SET PlaybackPositionTicks=0")
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(history.main(["snapshot", str(self.db), str(self.baseline)]), 1)

    def test_check_cli_reports_loss_and_returns_failure(self):
        history.main(["snapshot", str(self.db), str(self.baseline)])
        self.sql("UPDATE UserData SET Played=0,PlaybackPositionTicks=0")
        api = FakeApi(history.snapshot(self.db))
        with patch.object(history, "Api", return_value=api), contextlib.redirect_stdout(io.StringIO()) as output:
            self.assertEqual(history.main(["check", str(self.db), str(self.baseline),
                                           "--base-url", "http://fixture"]), 1)
        self.assertIn("API changed Played", output.getvalue())
        self.assertIn("API changed PlaybackPositionTicks", output.getvalue())
        self.assertNotIn(MOVIE, output.getvalue())


if __name__ == "__main__":
    unittest.main()
