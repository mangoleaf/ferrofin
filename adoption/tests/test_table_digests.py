"""Table-digest checks on invented rows: every listed table, each documented rewrite."""

import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).parents[1]))
import table_digests as digests

GUID = "0A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9"
SAME = GUID.replace("-", "").lower()
USER = "00000000-0000-0000-0000-0000000000a1"
GONE_USER = "00000000-0000-0000-0000-0000000000a2"
# Primary key and one data column per table; the real schemas have more columns.
KEYS = {
    "MediaStreamInfos": ("ItemId, StreamIndex", "Codec"), "AttachmentStreamInfos": ("ItemId, \"Index\"", "Filename"),
    "Chapters": ("ItemId, ChapterIndex", "Name, ImageDateModified"), "TrickplayInfos": ("ItemId, Width", "Height"),
    "MediaSegments": ("Id", "SegmentProviderId"), "KeyframeData": ("ItemId", "TotalDuration"),
    "ItemValues": ("ItemValueId", "Value, CleanValue"), "ItemValuesMap": ("ItemId, ItemValueId", "Extra"),
    "Peoples": ("Id", "Name"), "PeopleBaseItemMap": ("ItemId, PeopleId, Role", "SortOrder"),
    "DisplayPreferences": ("Id", "UserId, ScrollDirection"), "ItemDisplayPreferences": ("Id", "UserId, Client"),
    "CustomItemDisplayPreferences": ("Id", "UserId, \"Key\""), "Devices": ("Id", "UserId, DeviceId, AccessToken, DateCreated, DateModified, DateLastActivity"),
    "DeviceOptions": ("Id", "CustomName"), "ApiKeys": ("Id", "AccessToken, DateCreated, DateLastActivity"),
    "ActivityLogs": ("Id", "Name, DateCreated"),
}


class TableDigests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.db = str(Path(directory.name) / "jellyfin.db")
        script = 'CREATE TABLE Users(Id TEXT PRIMARY KEY); CREATE TABLE BaseItems(Id TEXT PRIMARY KEY, Path TEXT, OwnerId TEXT);'
        for table, (key, data) in KEYS.items():
            script += f'CREATE TABLE "{table}"({key}, {data}, PRIMARY KEY({key}));'
        self.sql(script + f"INSERT INTO Users VALUES ('{USER}');")
        for table, (key, data) in KEYS.items():
            columns = key.split(", ") + data.split(", ")
            values = ", ".join("'x'" if column.strip('"') != "UserId" else f"'{USER}'" for column in columns)
            self.sql(f'INSERT INTO "{table}"({", ".join(columns)}) VALUES ({values})')
        self.before = digests.snapshot(self.db)

    def sql(self, statement):
        conn = sqlite3.connect(self.db)
        try:
            conn.executescript(statement)
        finally:
            conn.close()

    def failures(self):
        return digests.compare(self.before, digests.snapshot(self.db, self.before))

    def test_every_listed_table_is_snapshotted_with_its_row(self):
        self.assertEqual(set(self.before["tables"]), set(digests.TABLES))
        self.assertTrue(all(len(table["rows"]) == 1 for table in self.before["tables"].values()))

    def test_unchanged_database_passes(self):
        self.assertEqual(self.failures(), [])

    def test_a_lost_row_fails_naming_table_and_key(self):
        for table in digests.TABLES:
            with self.subTest(table):
                self.sql(f'DELETE FROM "{table}"')
                self.assertEqual(len(self.failures()), 1)
                self.assertTrue(self.failures()[0].startswith(f"{table} row ") and self.failures()[0].endswith("is gone"))
                self.setUp()

    def test_a_changed_column_fails_naming_table_and_key(self):
        self.sql("UPDATE Chapters SET Name = 'other'")
        self.assertEqual(self.failures(), ["Chapters row x/x changed"])

    def test_a_changed_media_stream_column_is_hashed(self):
        self.sql("UPDATE MediaStreamInfos SET Codec = 'other'")
        self.assertEqual(self.failures(), ["MediaStreamInfos row x/x changed"])

    def test_rows_only_the_later_database_holds_pass(self):
        self.sql("INSERT INTO Peoples(Id, Name) VALUES ('new', 'Added by a scan')")
        self.assertEqual(self.failures(), [])

    def test_a_lost_column_is_named_once(self):
        self.sql("ALTER TABLE Chapters DROP COLUMN Name")
        self.assertEqual(self.failures(), ["Chapters lost column Name"])

    def test_a_column_added_by_a_migration_is_not_compared(self):
        self.sql("ALTER TABLE MediaStreamInfos ADD COLUMN IsOriginal INTEGER DEFAULT 0")
        self.assertEqual(self.failures(), [])

    def test_baseline_survives_json(self):
        self.before = json.loads(json.dumps(self.before))
        self.assertEqual(self.failures(), [])

    # The documented rewrites: each must pass, and the unrewritten change next to it must not.
    def test_guid_casing_and_dashes_are_one_identity_0007_0032(self):
        self.sql(f"UPDATE Chapters SET ItemId = '{GUID}'")
        self.before = digests.snapshot(self.db)
        self.sql(f"UPDATE Chapters SET ItemId = '{SAME.upper()}'")
        self.assertEqual(self.failures(), [])
        self.sql("UPDATE Chapters SET ItemId = 'not-a-guid'")
        self.assertEqual(len(self.failures()), 1)

    def test_timestamp_spelling_is_one_instant_0007(self):
        self.sql("UPDATE ActivityLogs SET DateCreated = '2026-01-02T03:04:05.5Z'")
        self.before = digests.snapshot(self.db)
        self.sql("UPDATE ActivityLogs SET DateCreated = '2026-01-02 03:04:05.500'")
        self.assertEqual(self.failures(), [])
        self.sql("UPDATE ActivityLogs SET DateCreated = '2026-01-02 03:04:06.500'")
        self.assertEqual(len(self.failures()), 1)

    def test_clean_value_is_rederived_0031(self):
        self.sql("UPDATE ItemValues SET CleanValue = 'rederived'")
        self.assertEqual(self.failures(), [])
        self.sql("UPDATE ItemValues SET Value = 'other'")
        self.assertEqual(len(self.failures()), 1)

    def test_activity_advances_last_activity_only(self):
        self.sql("UPDATE Devices SET DateLastActivity = 'later'; UPDATE ApiKeys SET DateLastActivity = 'later'")
        self.assertEqual(self.failures(), [])
        self.sql("UPDATE ApiKeys SET AccessToken = 'other'")
        self.assertEqual(self.failures(), ["ApiKeys row x changed"])

    def test_a_live_users_rows_are_baselined_whatever_the_guid_spelling(self):
        self.sql(f"UPDATE Users SET Id = '{USER.upper()}'")
        self.before = digests.snapshot(self.db)
        self.assertIn("x", self.before["tables"]["DisplayPreferences"]["rows"])
        self.sql("DELETE FROM DisplayPreferences")
        self.assertEqual(self.failures(), ["DisplayPreferences row x is gone"])

    def test_rows_of_a_deleted_user_are_cleaned_up_0044(self):
        for table in digests.USER_OWNED:
            self.sql(f"INSERT INTO \"{table}\"(Id, UserId) VALUES ('dangling', '{GONE_USER}')")
        self.before = digests.snapshot(self.db)
        for table in digests.USER_OWNED:
            self.assertEqual(len(self.before["tables"][table]["rows"]), 1)
            self.sql(f"DELETE FROM \"{table}\" WHERE Id = 'dangling'")
        self.assertEqual(self.failures(), [])
        # an attached row's loss still fails
        self.sql(f"DELETE FROM Devices WHERE UserId = '{USER}'")
        self.assertEqual(self.failures(), ["Devices row 000000000000000000000000000000a1/x is gone"])

    def test_a_rekeyed_item_is_the_same_item_by_its_path(self):
        self.sql(f"INSERT INTO BaseItems VALUES ('old', '/media/a.mkv', '{GUID}')")
        self.sql("UPDATE MediaStreamInfos SET ItemId = 'old'")
        self.before = digests.snapshot(self.db)
        self.sql("UPDATE BaseItems SET Id = 'new'; UPDATE MediaStreamInfos SET ItemId = 'new'")
        self.assertEqual(self.failures(), [])
        self.assertIn("path:/media/a.mkv/x", self.before["tables"]["MediaStreamInfos"]["rows"])

    def test_an_item_without_an_owner_is_not_reidentified(self):
        for owner in ("NULL", "'00000000-0000-0000-0000-000000000000'", "'00000000-0000-0000-0000-000000000001'"):
            self.sql(f"DELETE FROM BaseItems; INSERT INTO BaseItems VALUES ('old', '/media/a.mkv', {owner})")
            self.sql("UPDATE MediaStreamInfos SET ItemId = 'old'")
            self.before = digests.snapshot(self.db)
            self.sql("UPDATE BaseItems SET Id = 'new'; UPDATE MediaStreamInfos SET ItemId = 'new'")
            self.assertEqual(self.failures(), ["MediaStreamInfos row old/x is gone"], owner)

    def test_a_deleted_item_still_loses_its_rows_and_a_shared_path_identifies_nothing(self):
        self.sql(f"INSERT INTO BaseItems VALUES ('old', '/media/a.mkv', '{GUID}'), ('twin', '/media/a.mkv', '{GUID}')")
        self.sql("UPDATE MediaStreamInfos SET ItemId = 'old'")
        self.before = digests.snapshot(self.db)
        self.assertIn("old/x", self.before["tables"]["MediaStreamInfos"]["rows"])
        self.sql("DELETE FROM BaseItems; DELETE FROM MediaStreamInfos")
        self.assertEqual(self.failures(), ["MediaStreamInfos row old/x is gone"])

    def test_a_device_reannouncing_itself_passes(self):
        self.sql("ALTER TABLE Devices ADD COLUMN AppVersion TEXT; ALTER TABLE Devices ADD COLUMN DeviceName TEXT;")
        self.before = digests.snapshot(self.db)
        self.sql("UPDATE Devices SET AppVersion = '10.11.8', DeviceName = 'Chrome', DateModified = 'later'")
        self.assertEqual(self.failures(), [])

    def test_a_reauthenticated_device_may_be_replaced(self):
        # The fixture device's DeviceId is 'x'. A path that logs back in from it excludes it,
        # so a new Id, token and creation date pass; a device it did not log in from does not.
        reauth = {"x"}
        before = digests.snapshot(self.db, reauthenticated=reauth)
        self.sql("UPDATE Devices SET Id = 'new', AccessToken = 'fresh', DateCreated = 'now'")
        self.assertEqual(digests.compare(before, digests.snapshot(self.db, before, reauthenticated=reauth)), [])

    def test_a_reset_token_on_an_untouched_device_fails(self):
        # gl-667: a device the harness never logs in from must keep its exact token.
        self.sql("UPDATE Devices SET AccessToken = 'reset'")
        self.assertEqual(self.failures(), ["Devices row 000000000000000000000000000000a1/x changed"])

    def test_a_deactivated_device_on_an_untouched_device_fails(self):
        self.sql("ALTER TABLE Devices ADD COLUMN IsActive INTEGER DEFAULT 1")
        self.before = digests.snapshot(self.db)
        self.sql("UPDATE Devices SET IsActive = 0")
        self.assertEqual(self.failures(), ["Devices row 000000000000000000000000000000a1/x changed"])

    def test_a_device_moved_to_another_user_fails(self):
        self.sql(f"UPDATE Devices SET UserId = '{GONE_USER}'")
        # The baselined (UserId, DeviceId) key no longer exists.
        self.assertEqual(self.failures(), ["Devices row 000000000000000000000000000000a1/x is gone"])

    def test_two_rows_sharing_a_key_are_both_counted(self):
        # gl-668: (UserId, DeviceId) is not unique; two devices for one user+device must not
        # collapse into one row, or losing one would pass unseen.
        self.sql(f"INSERT INTO Devices(Id, UserId, DeviceId, AccessToken) VALUES ('second', '{USER}', 'x', 'other')")
        self.before = digests.snapshot(self.db)
        self.assertEqual(len(self.before["tables"]["Devices"]["rows"]["000000000000000000000000000000a1/x"]), 2)
        self.sql("DELETE FROM Devices WHERE Id = 'second'")
        failures = self.failures()
        self.assertEqual(len(failures), 1)
        self.assertEqual(
            failures[0], "Devices row 000000000000000000000000000000a1/x: 1 of 2 rows sharing its key gone or changed")

    def test_device_date_created_is_one_instant_0007(self):
        # 0007 rewrites DateCreated from the ISO 'T' form; an untouched device must not fail.
        self.sql("UPDATE Devices SET DateCreated = '2026-01-02T03:04:05.5Z'")
        self.before = digests.snapshot(self.db)
        self.sql("UPDATE Devices SET DateCreated = '2026-01-02 03:04:05.500'")
        self.assertEqual(self.failures(), [])
        self.sql("UPDATE Devices SET DateCreated = '2026-01-02 03:04:06.500'")
        self.assertEqual(len(self.failures()), 1)

    def test_a_reauthenticated_device_that_vanishes_still_fails(self):
        reauth = {"x"}
        before = digests.snapshot(self.db, reauthenticated=reauth)
        self.sql("DELETE FROM Devices")
        self.assertEqual(
            digests.compare(before, digests.snapshot(self.db, before, reauthenticated=reauth)),
            ["Devices row 000000000000000000000000000000a1/x is gone"])

    def test_reauthenticated_devices_is_exactly_the_harness_logins(self):
        self.assertEqual(
            digests.reauthenticated_devices(("admin", "adult"), "guest"),
            {"synthetic-login-admin", "synthetic-login-adult",
             "account-login-admin", "account-login-adult", "account-login-guest"})

    def test_compare_treats_rows_sharing_a_key_as_a_multiset(self):
        def snap(hashes):
            tables = {table: {"columns": [], "rows": {}} for table in digests.TABLES}
            tables["Devices"] = {"columns": ["UserId"], "rows": {"k": sorted(hashes)}}
            return {"version": digests.SNAPSHOT_VERSION, "tables": tables}
        # losing one of two distinct duplicates fails
        self.assertEqual(digests.compare(snap(["a", "b"]), snap(["a"])),
                         ["Devices row k: 1 of 2 rows sharing its key gone or changed"])
        # losing one of two byte-identical rows fails
        self.assertEqual(len(digests.compare(snap(["a", "a"]), snap(["a"]))), 1)
        # changing one of two duplicates fails
        self.assertEqual(len(digests.compare(snap(["a", "b"]), snap(["a", "c"]))), 1)
        # a duplicate only the later database holds passes
        self.assertEqual(digests.compare(snap(["a"]), snap(["a", "b"])), [])

    def test_cli_snapshot_then_check(self):
        baseline = self.db + ".tables.json"
        self.assertEqual(digests.main(["snapshot", self.db, baseline]), 0)
        self.assertEqual(digests.main(["check", self.db, baseline]), 0)
        self.sql("DELETE FROM Chapters")
        self.assertEqual(digests.main(["check", self.db, baseline]), 1)
        self.assertEqual(digests.main(["check", self.db, baseline + ".missing"]), 1)

    def test_cli_rejects_an_incompatible_baseline_version(self):
        baseline = self.db + ".tables.json"
        self.assertEqual(digests.main(["snapshot", self.db, baseline]), 0)
        stale = json.loads(Path(baseline).read_text())
        stale["version"] = 1
        Path(baseline).write_text(json.dumps(stale))
        self.assertEqual(digests.main(["check", self.db, baseline]), 1)


if __name__ == "__main__":
    unittest.main()
