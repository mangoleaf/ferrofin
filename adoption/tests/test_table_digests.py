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

    def test_a_rekeyed_items_metadata_folder_moves_with_its_id(self):
        old, new = "e24bb924be539d0746ec205b0b9536e6", "ad1cd20859f6c42cfdfd85714b08748d"
        image = "/config/metadata/library/{}/{}/chapters/1_0.jpg"
        self.sql(f"ALTER TABLE Chapters ADD COLUMN ImagePath TEXT; "
                 f"INSERT INTO BaseItems VALUES ('{old}', '/media/a - 720p.mkv', '{GUID}'); "
                 f"UPDATE Chapters SET ItemId = '{old}', ImagePath = '{image.format(old[:2], old)}'")
        self.before = digests.snapshot(self.db)
        self.sql(f"UPDATE BaseItems SET Id = '{new}'; "
                 f"UPDATE Chapters SET ItemId = '{new}', ImagePath = '{image.format(new[:2], new)}'")
        self.assertEqual(self.failures(), [])
        # Any other change to the path still shows: another file, or a folder left at the old id.
        for path in (image.format(new[:2], new).replace("1_0", "2_0"), image.format(old[:2], old)):
            self.sql(f"UPDATE Chapters SET ImagePath = '{path}'")
            self.assertEqual(self.failures(), ["Chapters row path:/media/a - 720p.mkv/x changed"], path)

    def test_an_item_not_reidentified_keeps_its_folder_id_compared(self):
        old, new = "e24bb924be539d0746ec205b0b9536e6", "ad1cd20859f6c42cfdfd85714b08748d"
        image = "/config/metadata/library/{}/{}/chapters/1_0.jpg"
        self.sql(f"ALTER TABLE Chapters ADD COLUMN ImagePath TEXT; "
                 f"INSERT INTO BaseItems VALUES ('{old}', '/media/a.mkv', NULL); "
                 f"UPDATE Chapters SET ItemId = '{old}', ImagePath = '{image.format(old[:2], old)}'")
        self.before = digests.snapshot(self.db)
        self.sql(f"UPDATE Chapters SET ImagePath = '{image.format(new[:2], new)}'")
        self.assertEqual(self.failures(), [f"Chapters row {old}/x changed"])

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

    def test_a_device_authenticating_again_is_the_same_device_but_not_another_users(self):
        self.sql("UPDATE Devices SET Id = 'new', AccessToken = 'fresh', DateCreated = 'now'")
        self.assertEqual(self.failures(), [])
        self.sql(f"UPDATE Devices SET UserId = '{GONE_USER}'")
        self.assertEqual(len(self.failures()), 1)

    def test_cli_snapshot_then_check(self):
        baseline = self.db + ".tables.json"
        self.assertEqual(digests.main(["snapshot", self.db, baseline]), 0)
        self.assertEqual(digests.main(["check", self.db, baseline]), 0)
        self.sql("DELETE FROM Chapters")
        self.assertEqual(digests.main(["check", self.db, baseline]), 1)
        self.assertEqual(digests.main(["check", self.db, baseline + ".missing"]), 1)


if __name__ == "__main__":
    unittest.main()
