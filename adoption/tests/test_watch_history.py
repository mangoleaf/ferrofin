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


PLACEHOLDER_ID = "00000000-0000-0000-0000-000000000001"
SERIES = "11111111-1111-1111-1111-111111111111"
SEASON = "22222222-2222-2222-2222-222222222222"
FILM = "33333333-3333-3333-3333-333333333333"
STILL = "44444444-4444-4444-4444-444444444444"
ORPHAN = "55555555-5555-5555-5555-555555555555"
ORPHAN_CHILD = "66666666-6666-6666-6666-666666666666"
ORPHAN_EXTRA = "77777777-7777-7777-7777-777777777777"
GONE = "88888888-8888-8888-8888-888888888888"
ADULT = "99999999-9999-9999-9999-999999999999"
COLUMNS = ("ItemId", "UserId", "CustomDataKey", "AudioStreamIndex", "IsFavorite", "LastPlayedDate",
           "Likes", "PlayCount", "PlaybackPositionTicks", "Played", "Rating", "SubtitleStreamIndex",
           "RetentionDate")


class AdoptionExpectationTest(unittest.TestCase):
    """The baseline models the two boot-time changes adoption makes to watch history:
    the harmonize repair (adoption_repairs.rs harmonize_conflicting_user_data, a port of
    Jellyfin 12.2's HarmonizeConflictingUserData) and migration 0042's orphan clean-up."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.db = Path(self.tmp.name) / "fixture.db"
        movie, episode = "MediaBrowser.Controller.Entities.Movies.Movie", "MediaBrowser.Controller.Entities.TV.Episode"
        self.sql(f'''
            CREATE TABLE BaseItems(Id TEXT PRIMARY KEY, Type TEXT, ParentId TEXT, OwnerId TEXT);
            CREATE TABLE UserData({", ".join(COLUMNS)}, PRIMARY KEY (ItemId, UserId, CustomDataKey));
            INSERT INTO BaseItems VALUES
              ('{PLACEHOLDER_ID}', 'MediaBrowser.Controller.Entities.Folder', NULL, NULL),
              ('{SERIES}', 'MediaBrowser.Controller.Entities.TV.Series', NULL, NULL),
              ('{SEASON}', 'MediaBrowser.Controller.Entities.TV.Season', '{SERIES}', NULL),
              ('{FILM}', '{movie}', NULL, NULL), ('{STILL}', '{movie}', NULL, NULL),
              ('{ORPHAN}', '{episode}', '{GONE}', NULL),
              ('{ORPHAN_CHILD}', '{episode}', '{ORPHAN}', NULL),
              ('{ORPHAN_EXTRA}', 'MediaBrowser.Controller.Entities.Video', NULL, '{ORPHAN_CHILD}');
        ''')

    def sql(self, statement, *args):
        with contextlib.closing(sqlite3.connect(self.db)) as db, db:
            if args:
                db.execute(statement, args)
            else:
                db.executescript(statement)

    def play(self, key, date, count, ticks, played, item=FILM, **extra):
        row = {"ItemId": item, "UserId": ADULT, "CustomDataKey": key, "IsFavorite": 0,
               "LastPlayedDate": date, "PlayCount": count, "PlaybackPositionTicks": ticks,
               "Played": int(played), **extra}
        self.sql(f"INSERT INTO UserData ({', '.join(row)}) VALUES ({', '.join('?' * len(row))})",
                 *row.values())

    def adopt(self, key, winner):
        """What the Rust repair writes: the winner's nine columns onto the row `key`."""
        nine = ", ".join(f"{column} = (SELECT {column} FROM UserData WHERE CustomDataKey = ?)"
                         for column in history.HARMONIZED)
        self.sql(f"UPDATE UserData SET {nine} WHERE CustomDataKey = ?", *([winner] * 9), key)

    def rows(self):
        with contextlib.closing(sqlite3.connect(self.db)) as db:
            return db.execute("SELECT CustomDataKey, Played, PlayCount, PlaybackPositionTicks, "
                              "LastPlayedDate FROM UserData ORDER BY CustomDataKey").fetchall()

    def test_conflicting_rows_collapse_to_the_most_recent_play(self):
        # Upstream's Perform_ConflictingRows_CollapsesToMostRecentPlay.
        self.play("497698", "2023-08-14 00:00:00", 9, 0, True)
        self.play("tt3480822", "2021-12-31 00:00:00", 7, 490, False)
        self.play("agrees-1", "2024-01-01 00:00:00", 1, 77, False, item=STILL)
        self.play("agrees-2", "2024-01-01 00:00:00", 1, 77, False, item=STILL)
        expected = history.snapshot(self.db, expected=True)
        # The unrepaired database is not what adoption must leave.
        self.assertEqual(history.compare_db(expected, history.snapshot(self.db)),
                         {"database changed watch-history row": 1})
        self.adopt("tt3480822", "497698")
        self.assertFalse(history.compare_db(expected, history.snapshot(self.db)))
        self.assertEqual(self.rows()[-1], ("tt3480822", 1, 9, 0, "2023-08-14 00:00:00"))
        film = expected["items"][history.item_id(ADULT) + ":" + history.item_id(FILM)]
        self.assertEqual((film["Played"], film["PlayCount"], film["PlaybackPositionTicks"]), (1, 9, 0))

    def test_a_row_that_diverges_after_adoption_still_fails(self):
        # Only the baseline is harmonized: a group that agrees before adoption and
        # disagrees after it is a loss, not something to repair in the comparison.
        self.play("497698", "2023-08-14 00:00:00", 9, 0, True)
        self.play("tt3480822", "2023-08-14 00:00:00", 9, 0, True)
        expected = history.snapshot(self.db, expected=True)
        self.sql("UPDATE UserData SET Played = 0 WHERE CustomDataKey = 'tt3480822'")
        self.assertEqual(history.compare_db(expected, history.snapshot(self.db)),
                         {"database changed watch-history row": 1})

    def test_a_null_date_loses_and_a_full_tie_goes_to_the_lower_key(self):
        self.play("dated", "2020-01-01 00:00:00", 1, 0, False)
        # Unplayed: migration 0010 stamps a date on played rows that have none.
        self.play("never", None, 50, 900, False)
        self.play("tie-b", "2022-02-02 00:00:00", 3, 30, False, item=STILL, IsFavorite=1)
        self.play("tie-a", "2022-02-02 00:00:00", 3, 30, False, item=STILL, IsFavorite=0)
        expected = history.snapshot(self.db, expected=True)
        self.adopt("never", "dated")
        self.adopt("tie-b", "tie-a")
        self.assertFalse(history.compare_db(expected, history.snapshot(self.db)))

    def test_dates_compare_by_instant_exact_to_the_tick(self):
        # The existing timestamp() keeps microseconds only, so it cannot tell these apart;
        # the repair can, and so must the expectation.
        ef, ferrofin = "2026-08-08 19:14:59.474065", "2026-08-08 19:14:59.4740651"
        self.assertEqual(history.timestamp(ef), history.timestamp(ferrofin))
        self.assertLess(history.instant(ef), history.instant(ferrofin))
        self.assertEqual(history.instant("2023-08-14 00:00:00"), history.instant("2023-08-14T00:00:00.0000000Z"))
        self.play("ef", ef, 1, 10, False)
        self.play("ferrofin", ferrofin, 1, 10, False, Likes=1)
        expected = history.snapshot(self.db, expected=True)
        self.adopt("ef", "ferrofin")
        self.assertFalse(history.compare_db(expected, history.snapshot(self.db)))

    def test_one_instant_written_two_ways_is_no_conflict(self):
        # EF trims trailing fraction zeros; Ferrofin writes seven digits. Likes differs,
        # but it is not one of the five fields the repair's query compares.
        self.play("ef", "2026-05-14 17:35:09.533", 1, 10, False)
        self.play("ferrofin", "2026-05-14 17:35:09.5330000", 1, 10, False, Likes=1)
        self.assertFalse(history.compare_db(history.snapshot(self.db, expected=True),
                                            history.snapshot(self.db)))

    def test_the_winners_date_text_is_copied_verbatim(self):
        self.play("497698", "2023-08-14 00:00:00", 9, 0, True)
        self.play("tt3480822", "2021-12-31 00:00:00", 7, 490, False)
        expected = history.snapshot(self.db, expected=True)
        self.adopt("tt3480822", "497698")
        self.sql("UPDATE UserData SET LastPlayedDate = '2023-08-14 00:00:00.0000000' "
                 "WHERE CustomDataKey = 'tt3480822'")
        self.assertEqual(history.compare_db(expected, history.snapshot(self.db)),
                         {"database changed watch-history row": 1})

    def test_placeholder_rows_are_never_harmonized(self):
        # Upstream's Perform_DetachedRows_AreLeftAlone.
        self.play("497698", "2023-08-14 00:00:00", 9, 0, True, item=PLACEHOLDER_ID, RetentionDate="2026-01-01 00:00:00")
        self.play("tt3480822", "2021-12-31 00:00:00", 7, 490, False, item=PLACEHOLDER_ID, RetentionDate="2026-01-01 00:00:00")
        self.assertFalse(history.compare_db(history.snapshot(self.db, expected=True), history.snapshot(self.db)))

    def test_a_detached_rows_retention_date_must_not_change(self):
        # Pre-existing placeholder rows keep their exact retention clock; only rows 0042
        # moves there get a new one.
        self.play("497698", "2023-08-14 00:00:00", 9, 0, True, item=PLACEHOLDER_ID, RetentionDate="2026-01-01 00:00:00")
        expected = history.snapshot(self.db, expected=True)
        self.assertEqual(expected["moved"], [])
        self.sql(f"UPDATE UserData SET RetentionDate = '2026-10-12 08:00:00.0000000' WHERE ItemId = '{PLACEHOLDER_ID}'")
        self.assertEqual(history.compare_db(expected, history.snapshot(self.db)),
                         {"database changed watch-history row": 1})

    def test_orphaned_items_include_descendants_and_owned_extras(self):
        with contextlib.closing(history.connect(self.db)) as conn:
            self.assertEqual(history.orphaned_items(conn),
                             {history.item_id(i) for i in (ORPHAN, ORPHAN_CHILD, ORPHAN_EXTRA)})

    def test_orphaned_history_is_expected_on_the_placeholder(self):
        self.play("orphan-key", "2025-05-05 00:00:00", 3, 0, True, item=ORPHAN_CHILD)
        self.play("kept", "2025-01-01 00:00:00", 1, 0, True, item=ORPHAN, IsFavorite=1)
        self.play("kept", "2024-01-01 00:00:00", 1, 0, True, item=ORPHAN_CHILD)
        self.play("kept", "2020-01-01 00:00:00", 1, 0, False, item=PLACEHOLDER_ID, RetentionDate="2020-02-02 00:00:00")
        expected = history.snapshot(self.db, expected=True)
        self.assertNotIn(history.item_id(ADULT) + ":" + history.item_id(ORPHAN_CHILD), expected["items"])
        # What 0042 does: the placeholder's colliding row goes, the first orphan row per
        # key moves with a RetentionDate, the rest go, and so do the items.
        self.sql(f"""
            DELETE FROM UserData WHERE ItemId = '{PLACEHOLDER_ID}' AND CustomDataKey = 'kept';
            DELETE FROM UserData WHERE ItemId = '{ORPHAN_CHILD}' AND CustomDataKey = 'kept';
            UPDATE UserData SET ItemId = '{PLACEHOLDER_ID}', RetentionDate = '2026-10-12 08:00:00.0000000'
              WHERE ItemId IN ('{ORPHAN}', '{ORPHAN_CHILD}');
            DELETE FROM BaseItems WHERE Id IN ('{ORPHAN}', '{ORPHAN_CHILD}', '{ORPHAN_EXTRA}');
        """)
        self.assertFalse(history.compare_db(expected, history.snapshot(self.db)))
        self.sql("UPDATE UserData SET RetentionDate = NULL WHERE CustomDataKey = 'orphan-key'")
        self.assertEqual(history.compare_db(expected, history.snapshot(self.db)),
                         {"database changed watch-history row": 1})
        self.sql("DELETE FROM UserData WHERE CustomDataKey = 'orphan-key'")
        self.assertEqual(history.compare_db(expected, history.snapshot(self.db)),
                         {"database missing watch-history row": 1})

    def test_an_orphan_left_in_place_fails(self):
        self.play("orphan-key", "2025-05-05 00:00:00", 3, 0, True, item=ORPHAN_CHILD)
        expected = history.snapshot(self.db, expected=True)
        self.assertEqual(history.compare_db(expected, history.snapshot(self.db)),
                         {"database missing watch-history row": 1})


if __name__ == "__main__":
    unittest.main()
