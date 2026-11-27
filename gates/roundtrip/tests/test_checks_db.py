"""checks_db.py and allowlist.py on databases with Jellyfin's pinned schema (no docker, no server): the export's
schema and loss report, the copy, the row diff and its explanations, the item counts, and the allowlist's own check."""

from contextlib import closing, redirect_stdout
import gzip
import io
import json
import shutil
import sqlite3
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.append(str(Path(__file__).resolve().parent.parent))
sys.path.append(str(Path(__file__).resolve().parent))
import allowlist  # noqa: E402
import checks_db  # noqa: E402
from allowlist import Rule  # noqa: E402
from fake_server import SCHEMA, insert  # noqa: E402

MOVIE = "MediaBrowser.Controller.Entities.Movies.Movie"


class Databases(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)

    def jellyfin(self, name, *rows):
        """A database with Jellyfin's schema at tmp/name, holding `rows`: (table, {column: value})."""
        path = self.tmp / name
        with closing(sqlite3.connect(path)) as db:
            db.execute("PRAGMA synchronous = OFF")   # a throwaway file: no flush per statement
            db.executescript(SCHEMA.read_text(encoding="utf-8"))
            for table, values in rows:
                insert(db, table, **values)
            db.commit()
        return path

    def edit(self, path, *sql):
        with closing(sqlite3.connect(path)) as db:
            for statement in sql:
                db.execute(statement)
            db.commit()


def item(key, name="A movie", kind=MOVIE, path=None, created="2026-01-01 00:00:00", **more):
    return "BaseItems", {"Id": key, "Type": kind, "Name": name, "Path": path or f"/media/movies/{name}.mkv",
                         "DateCreated": created, **more}


class Export(Databases):
    def run_export(self, report_text, source_sql=("CREATE TABLE FerrofinMeta (Key TEXT)",
                                                  "INSERT INTO FerrofinMeta VALUES ('a'), ('b')")):
        exported, source = self.jellyfin("exported.db"), self.jellyfin("source.db")
        self.edit(source, *source_sql)
        report = self.tmp / "report.txt"
        report.write_text(report_text, encoding="utf-8")
        return exported, source, report

    def test_jellyfin_s_schema_and_every_table_reported_pass(self):
        exported, source, report = self.run_export("What the export left behind\n\n  FerrofinMeta (2 rows): settings.\n")
        self.assertEqual(checks_db.export(exported, source, report, SCHEMA), [])

    def test_a_table_with_rows_missing_from_the_report_or_with_another_count_is_named(self):
        for text in ("Nothing\n", "  FerrofinMeta (1 row): settings.\n"):
            exported, source, report = self.run_export(text)
            self.assertEqual(checks_db.export(exported, source, report, SCHEMA), [
                "loss report: 1 table(s) of Ferrofin's held rows and are not in it with their count, first: FerrofinMeta (2)"])
            shutil.rmtree(self.tmp)
            self.tmp.mkdir()

    def test_a_source_with_no_table_of_ferrofin_s_is_not_a_pass(self):
        exported, source, report = self.run_export("", source_sql=())
        self.assertEqual(checks_db.export(exported, source, report, SCHEMA),
                         ["the database the export read holds no table of Ferrofin's: nothing to find in the loss report"])

    def test_a_table_rebuilt_without_autoincrement_or_in_another_column_order_is_named(self):
        def rebuilt(columns, key):   # in a fresh tmp each time
            shutil.rmtree(self.tmp)
            self.tmp.mkdir()
            exported, source, report = self.run_export("  FerrofinMeta (2 rows): settings.\n")
            self.edit(exported, "DROP TABLE AccessSchedules",
                      f'CREATE TABLE "AccessSchedules" ({columns}, CONSTRAINT "PK_AccessSchedules" PRIMARY KEY ("Id"{key}))',
                      'CREATE INDEX "IX_AccessSchedules_UserId" ON "AccessSchedules" ("UserId")')
            return checks_db.export(exported, source, report, SCHEMA)

        jellyfin_order = ('"Id" INTEGER NOT NULL, "UserId" TEXT NOT NULL, "DayOfWeek" INTEGER NOT NULL, '
                          '"StartHour" REAL NOT NULL, "EndHour" REAL NOT NULL')
        another_order = ('"Id" INTEGER NOT NULL, "DayOfWeek" INTEGER NOT NULL, "EndHour" REAL NOT NULL, '
                         '"StartHour" REAL NOT NULL, "UserId" TEXT NOT NULL')
        self.assertEqual(rebuilt(jellyfin_order, " AUTOINCREMENT"), [])   # Jellyfin's table again: nothing to name
        for columns, key in ((jellyfin_order, ""), (another_order, " AUTOINCREMENT")):   # one difference each
            problems = rebuilt(columns, key)
            self.assertEqual(len(problems), 2, problems)
            self.assertIn("table AccessSchedules", problems[0])
            self.assertIn("table AccessSchedules", problems[1])
            self.assertEqual(("'AUTOINCREMENT'" in problems[0], "'AUTOINCREMENT'" in problems[1]), (bool(key), True))

    def test_an_object_left_behind_or_missing_is_named(self):
        exported, source, report = self.run_export("  FerrofinMeta (2 rows): settings.\n")
        self.edit(exported, "CREATE TABLE FerrofinMeta (Key TEXT)", "DROP INDEX IX_BaseItems_Type_TopParentId_Id")
        problems = checks_db.export(exported, source, report, SCHEMA)
        self.assertEqual(len(problems), 2, problems)
        self.assertTrue(problems[0].startswith("schema: 1 object(s) Jellyfin's schema does not have, first: table "
                                               "FerrofinMeta"), problems)
        self.assertIn("index IX_BaseItems_Type_TopParentId_Id", problems[1])


class Snapshot(Databases):
    def test_a_copy_holds_the_rows_and_never_overwrites(self):
        live = self.jellyfin("live.db", item("A"))
        checks_db.snapshot(live, self.tmp / "copy.db")
        with closing(sqlite3.connect(self.tmp / "copy.db")) as db:
            self.assertEqual(db.execute("SELECT Id FROM BaseItems").fetchall(), [("A",)])
        with self.assertRaises(FileExistsError):
            checks_db.snapshot(live, self.tmp / "copy.db")
        with self.assertRaises(sqlite3.OperationalError):   # a missing database is never made empty
            checks_db.snapshot(self.tmp / "missing.db", self.tmp / "other.db")
        self.assertFalse((self.tmp / "missing.db").exists())


class Rows(Databases):
    RULES = (
        Rule("saved", ("scan",), "BaseItems", "changed", "timestamp", "stamped on every save", columns=("DateLastSaved",)),
        Rule("data", ("scan",), "BaseItems", "changed", "timestamp", "the saved item", columns=("Data",),
             keys=("DateLastSaved",)),
        Rule("bandwidth", ("scan",), "TrickplayInfos", "changed", "allowlisted", "the owner's 5% (gl-1596)",
             columns=("Bandwidth",), within=0.05),
        Rule("gate-device", ("capture",), "Devices", "added", "gate", "the gate's sign-in",
             where=(("DeviceId", "roundtrip-.*"),)),
        Rule("image-id", ("scan",), "BaseItemImageInfos", "changed", "timestamp", "a new id per save", columns=("Id",)),
        Rule("gone-log", ("scan",), "ActivityLogs", "removed", "timestamp", "the log's own clean-up",
             where=(("Type", "Old"),)),
    )

    def diff(self, window, before, after, rules=None):
        out = self.tmp / f"rows-{window}.txt"
        problems = checks_db.rows(window, before, after, out, rules or self.RULES)
        with gzip.open(f"{out}.jsonl.gz", "rt", encoding="utf-8") as record:
            return problems, out.read_text(encoding="utf-8"), [json.loads(line) for line in record]

    def pair(self, *rows):
        return self.jellyfin("before.db", *rows), self.jellyfin("after.db", *rows)

    def test_equal_copies_change_nothing(self):
        before, after = self.pair(item("A"))
        problems, text, record = self.diff("scan", before, after)
        self.assertEqual((problems, record), ([], []))
        self.assertIn("unexplained (0):", text)

    def test_a_change_every_column_of_which_an_entry_covers_is_explained(self):
        before, after = self.pair(item("A", DateLastSaved="1", Data='{"DateLastSaved": "1", "Name": "x"}'),
                                  ("TrickplayInfos", {"ItemId": "A", "Width": 320, "Bandwidth": 1000}))
        self.edit(after, """UPDATE BaseItems SET DateLastSaved = '2', Data = '{"Name": "x", "DateLastSaved": "2"}'""",
                  "UPDATE TrickplayInfos SET Bandwidth = 1049")
        problems, text, record = self.diff("scan", before, after)
        self.assertEqual(problems, [])
        self.assertEqual(sorted((r["table"], [(e["kind"], e["name"]) for e in r["explained_by"]]) for r in record),
                         [("BaseItems", [("timestamp", "saved"), ("timestamp", "data")]),
                          ("TrickplayInfos", [("allowlisted", "bandwidth")])])
        self.assertEqual(record[0]["explained_by"][0]["cause"], "stamped on every save")
        self.assertIn("  BaseItems changed Data,DateLastSaved: timestamp:saved timestamp:data 1", text)

    def test_a_column_no_entry_covers_is_unexplained_and_named_with_its_values(self):
        before, after = self.pair(item("A", DateLastSaved="1", Data='{"DateLastSaved": "1", "Name": "x"}'),
                                  ("TrickplayInfos", {"ItemId": "A", "Width": 320, "Bandwidth": 1000}))
        self.edit(after, "UPDATE BaseItems SET DateLastSaved = '2', Name = 'B', "
                         """Data = '{"DateLastSaved": "2", "Name": "y"}'""",
                  "UPDATE TrickplayInfos SET Bandwidth = 1051")   # past the 5%
        problems, text, record = self.diff("scan", before, after)
        self.assertEqual(problems, ["2 row(s) the scan changed that no allowlist entry explains: BaseItems 1 changed, "
                                    "TrickplayInfos 1 changed (rows-scan.txt)"])
        self.assertIn('      Name: "A movie" -> "B"', text)
        self.assertIn('      Data: DateLastSaved: "1" -> "2"; Name: "x" -> "y"', text)   # a JSON object, by key
        self.assertIn("      Bandwidth: 1000 -> 1051", text)
        self.assertEqual(next(r for r in record if r["table"] == "BaseItems")["unexplained"], ["Data", "Name"])

    def test_an_entry_applies_only_in_its_windows_and_to_the_rows_it_names(self):
        before, after = self.pair()
        self.edit(after, "INSERT INTO Devices (UserId, AccessToken, AppName, AppVersion, DeviceName, DeviceId, IsActive, "
                         "DateCreated, DateModified, DateLastActivity) VALUES ('u', 't', 'a', 'v', 'n', 'roundtrip-x', 1, "
                         "'d', 'd', 'd'), ('u', 't2', 'a', 'v', 'n', 'web', 1, 'd', 'd', 'd')")
        problems, text, _ = self.diff("capture", before, after)
        self.assertEqual(problems, ["1 row(s) the capture changed that no allowlist entry explains: Devices 1 added "
                                    "(rows-capture.txt)"])
        self.assertIn('"DeviceId": "web"', text)
        problems, _, _ = self.diff("scan", before, after)   # the gate's entry is the capture's only
        self.assertIn("2 row(s) the scan changed", problems[0])

    def test_a_json_object_written_again_with_no_key_changed_is_no_stamp_s_doing(self):
        before, after = self.pair(item("A", Data='{"DateLastSaved": "1", "Name": "x"}'))
        self.edit(after, """UPDATE BaseItems SET Data = '{"Name":"x","DateLastSaved":"1"}'""")
        problems, text, _ = self.diff("scan", before, after)
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("      Data: the same JSON object, written in another order or spacing", text)

    def test_the_save_and_refresh_stamps_explain_nothing_in_a_second_start(self):
        for name in ("saved", "saved-in-data", "refreshed", "image-id"):
            rule = next(r for r in allowlist.RULES if r.name == name)
            self.assertIn("scan", rule.windows, name)
            self.assertNotIn("restart", rule.windows, name)

    def test_the_artefact_entries_explain_the_rows_jellyfin_writes_for_the_gate_and_nothing_else(self):
        """The production entries on rows shaped as Jellyfin 12.4 wrote them in a round trip's capture window."""
        def explained(window, table, change, before=None, after=None, changed=()):
            used, left = checks_db.explain(allowlist.RULES, window, table, change, list(changed), before, after)
            return [rule.name for rule in used] if not left else None

        user = {"Username": "roundtrip-user", "LastLoginDate": "1", "LastActivityDate": "1", "RowVersion": 1}
        signed_in = {**user, "LastLoginDate": "2", "LastActivityDate": "2", "RowVersion": 2}
        cols = ("LastLoginDate", "LastActivityDate", "RowVersion")
        self.assertEqual(explained("capture", "Users", "changed", user, signed_in, cols), ["gate-sign-in"])
        self.assertIsNone(explained("scan", "Users", "changed", user, signed_in, cols))   # no sign-in of the gate's there
        other = {**user, "Username": "synthetic-admin"}
        self.assertIsNone(explained("capture", "Users", "changed", other, {**signed_in, "Username": "synthetic-admin"},
                                    cols))   # another account signs in nowhere in the gate
        self.assertEqual(explained("capture", "Users", "changed", other, {**other, "LastActivityDate": "2"},
                                   ("LastActivityDate",)), ["user-activity"])
        device = {"DeviceId": "roundtrip-roundtrip-user", "DateLastActivity": "1"}
        self.assertEqual(explained("capture", "Devices", "removed", before=device), ["gate-device-gone"])
        self.assertEqual(explained("stop", "Devices", "added", after=device), ["gate-device-new"])
        self.assertIsNone(explained("capture", "Devices", "removed", before={**device, "DeviceId": "web-browser"}))
        self.assertEqual(explained("capture", "Devices", "changed", {**device, "DeviceId": "web"},
                                   {"DeviceId": "web", "DateLastActivity": "2"}, ("DateLastActivity",)),
                         ["device-activity"])
        failed = {"Type": "AuthenticationFailed", "Name": "Failed login attempt from roundtrip-user"}
        self.assertEqual(explained("capture", "ActivityLogs", "added", after=failed), ["gate-sign-in-log"])
        self.assertIsNone(explained("capture", "ActivityLogs", "added", after={**failed, "Name": "Failed login "
                                                                                     "attempt from synthetic-admin"}))
        self.assertEqual(explained("capture", "ActivityLogs", "added",
                                   after={"Type": "SessionStarted", "Name": "synthetic-admin is online from Chrome"}),
                         ["session-log"])
        self.assertIsNone(explained("capture", "ActivityLogs", "added", after={"Type": "TaskFailed", "Name": "x"}))
        self.assertIsNone(explained("capture", "ActivityLogs", "removed",
                                    before={"Type": "SessionStarted", "Name": "y"}))

    def test_an_entry_names_the_type_of_an_image_row_s_item(self):
        """ITEM_TYPE: a row of a table with an ItemId is matched by its item's BaseItems Type, read from the copies."""
        rules = (Rule("collage", ("capture",), "BaseItemImageInfos", "changed", "gate", "a playlist's collage drawn again",
                      columns=("DateModified",), where=((allowlist.ITEM_TYPE, r".*\.Playlist"),)),)
        image = {"ImageType": 0, "DateModified": "1"}
        before, after = self.pair(item("P", "A playlist", kind="MediaBrowser.Controller.Playlists.Playlist"), item("M"),
                                  ("BaseItemImageInfos", {"Id": "I1", "ItemId": "p", "Path": "/config/p.png", **image}),
                                  ("BaseItemImageInfos", {"Id": "I2", "ItemId": "M", "Path": "/config/m.jpg", **image}),
                                  ("BaseItemImageInfos", {"Id": "I3", "ItemId": "GONE", "Path": "/config/g.png", **image}))
        self.edit(after, "UPDATE BaseItemImageInfos SET DateModified = '2'")
        problems, _, record = self.diff("capture", before, after, rules)
        self.assertEqual(problems, ["2 row(s) the capture changed that no allowlist entry explains: BaseItemImageInfos 2 "
                                    "changed (rows-capture.txt)"])
        # the item found whatever the id's case; a movie's image, and one whose item no copy holds, not explained
        self.assertEqual(sorted((r["key"], [e["name"] for e in r["explained_by"]]) for r in record),
                         [(["I1"], ["collage"]), (["I2"], []), (["I3"], [])])
        self.assertNotIn(allowlist.ITEM_TYPE, record[0]["before"])   # matched on, never recorded as a column

    def test_the_audit_and_gate_entries_explain_only_the_rows_they_name(self):
        """The production entries gl-1573 added, on rows shaped as a round trip wrote them."""
        def explained(window, table, change, before=None, after=None, changed=(), item_type=None):
            used, left = checks_db.explain(allowlist.RULES, window, table, change, list(changed), before, after, item_type)
            return [rule.name for rule in used] if not left else None

        playlist = "MediaBrowser.Controller.Playlists.Playlist"
        collage = {"ImageType": 0, "DateModified": "1", "Blurhash": b"a"}
        drawn = {**collage, "DateModified": "2", "Blurhash": b"b"}
        self.assertEqual(explained("capture", "BaseItemImageInfos", "changed", collage, drawn,
                                   ("DateModified", "Blurhash"), playlist), ["playlist-collage", "blurhash"])
        for window, kind, image_type in (("scan", playlist, 0), ("capture", MOVIE, 0), ("capture", playlist, 2)):
            self.assertIsNone(explained(window, "BaseItemImageInfos", "changed", {**collage, "ImageType": image_type},
                                        {**drawn, "ImageType": image_type}, ("DateModified",), kind), (window, kind))
        for window in ("capture", "scan"):
            self.assertEqual(explained(window, "BaseItemImageInfos", "changed", collage, {**collage, "Blurhash": b"b"},
                                       ("Blurhash",), MOVIE), ["blurhash"], window)
        for window in ("stop", "restart"):   # Jellyfin measured images again only at the gate's requests and its scan
            self.assertIsNone(explained(window, "BaseItemImageInfos", "changed", collage,
                                        {**collage, "Blurhash": b"b"}, ("Blurhash",), MOVIE), window)
        for old, new in ((None, b"a"), (b"", b"a"), (b"a", None), (b"a", b"")):   # filled in or lost: no gl-1058 case
            self.assertIsNone(explained("capture", "BaseItemImageInfos", "changed", {**collage, "Blurhash": old},
                                        {**collage, "Blurhash": new}, ("Blurhash",), MOVIE), (old, new))
        self.assertIsNone(explained("capture", "BaseItemImageInfos", "changed", collage, drawn,
                                    ("DateModified",), None))   # an image of no item either copy holds
        seeded = {"Type": playlist, "Name": "Synthetic linked playlist", "RunTimeTicks": 80000000}
        self.assertEqual(explained("capture", "BaseItems", "changed", seeded, {**seeded, "RunTimeTicks": 320000000},
                                   ("RunTimeTicks",)), ["playlist-runtime"])
        for name in ("Round trip playlist renamed", "Synthetic private playlist"):   # made or emptied in Ferrofin
            made = {**seeded, "Name": name}
            self.assertIsNone(explained("capture", "BaseItems", "changed", made, {**made, "RunTimeTicks": 1},
                                        ("RunTimeTicks",)), name)
        for value in (0, None):   # a sum of the members, never none
            self.assertIsNone(explained("capture", "BaseItems", "changed", seeded, {**seeded, "RunTimeTicks": value},
                                        ("RunTimeTicks",)), value)
        self.assertIsNone(explained("capture", "BaseItems", "changed", {**seeded, "RunTimeTicks": 1}, seeded,
                                    ("RunTimeTicks",)))   # the fixtures' stale value only
        movie = {**seeded, "Type": MOVIE}
        self.assertIsNone(explained("capture", "BaseItems", "changed", movie, {**movie, "RunTimeTicks": 1},
                                    ("RunTimeTicks",)))
        self.assertIsNone(explained("scan", "BaseItems", "changed", seeded, {**seeded, "RunTimeTicks": 320000000},
                                    ("RunTimeTicks",)))

    def test_a_removed_row_is_explained_only_where_its_entry_applies(self):
        log = {"Name": "x", "UserId": "u", "DateCreated": "d", "LogSeverity": 1}
        before, after = self.pair(("ActivityLogs", {"Id": 1, "Type": "Old", **log}),
                                  ("ActivityLogs", {"Id": 2, "Type": "New", **log}))
        self.edit(after, "DELETE FROM ActivityLogs")
        problems, _, record = self.diff("scan", before, after)
        self.assertEqual(problems, ["1 row(s) the scan changed that no allowlist entry explains: ActivityLogs 1 removed "
                                    "(rows-scan.txt)"])
        self.assertEqual([(r["key"], [e["name"] for e in r["explained_by"]]) for r in record],
                         [([1], ["gone-log"]), ([2], [])])

    def test_a_blob_is_compared_and_shown_as_hex(self):
        image = {"ItemId": "A", "ImageType": 0, "Path": "/config/metadata/a.jpg", "DateModified": "1"}
        before, after = self.pair(item("A"), ("BaseItemImageInfos", {"Id": "I", "Blurhash": b"LEHV", **image}))
        self.edit(after, "UPDATE BaseItemImageInfos SET Blurhash = X'00ff'")
        problems, text, record = self.diff("scan", before, after)
        self.assertEqual(len(problems), 1)
        self.assertIn('      Blurhash: {"blob": "4c454856"} -> {"blob": "00ff"}', text)
        self.assertEqual(record[0]["after"]["Blurhash"], {"blob": "00ff"})

    def test_a_case_only_rewrite_is_a_change(self):
        before, after = self.pair(item("A", Name="Movie"))
        self.edit(after, "UPDATE BaseItems SET Name = 'movie'")
        problems, _, record = self.diff("scan", before, after)
        self.assertEqual([r["columns"] for r in record], [["Name"]])
        self.assertEqual(len(problems), 1)

    def test_an_image_row_saved_again_under_a_new_id_is_paired_by_item_type_and_path(self):
        image = {"ItemId": "A", "ImageType": 0, "Path": "/config/metadata/a.jpg", "DateModified": "1"}
        before, after = self.pair(item("A"), ("BaseItemImageInfos", {"Id": "OLD", **image}))
        self.edit(after, "UPDATE BaseItemImageInfos SET Id = 'NEW'")
        problems, _, record = self.diff("scan", before, after)
        self.assertEqual((problems, [(r["change"], r["columns"]) for r in record]), ([], [("changed", ["Id"])]))
        self.edit(after, "UPDATE BaseItemImageInfos SET Path = '/config/metadata/b.jpg'")   # another image: not paired
        _, _, record = self.diff("scan", before, after)
        self.assertEqual(sorted(r["change"] for r in record), ["added", "removed"])

    def test_a_table_only_one_copy_has_and_a_copy_with_no_table_are_problems(self):
        before, after = self.pair()
        self.edit(after, "CREATE TABLE Extra (Id INTEGER)")
        problems, _, _ = self.diff("scan", before, after)
        self.assertEqual(problems, ["the scan: 1 table(s) only after it, first: Extra"])
        empty = self.tmp / "empty.db"
        sqlite3.connect(empty).close()
        self.assertEqual(checks_db.rows("scan", empty, after, self.tmp / "x.txt"),
                         [f"the scan: no table in {empty} to compare"])
        with self.assertRaises(sqlite3.OperationalError):
            checks_db.rows("scan", before, self.tmp / "missing.db", self.tmp / "x.txt")


class Counts(Databases):
    def test_the_same_items_pass(self):
        before = self.jellyfin("before.db", item("A"), item("B", "Another"))
        self.assertEqual(checks_db.counts(before, before), [])

    def test_a_removed_a_re_added_and_a_recounted_item_are_named(self):
        before = self.jellyfin("before.db", item("A"), item("B", "Another"), item("C", "Third"))
        after = self.jellyfin("after.db", item("A", created="2026-02-02 00:00:00"), item("D", "Another"),
                              item("E", "New", kind="MediaBrowser.Controller.Entities.Folder", path="/media/new"))
        self.assertEqual(checks_db.counts(before, after), [
            "item counts by type changed: Folder 0 -> 1, Movie 3 -> 2",
            "2 item(s) removed, first: Another (Movie), Third (Movie)",
            "2 item(s) re-added (another id at a removed item's path, or another DateCreated), first: Another (Movie), "
            "A movie (Movie)"])

    def test_no_item_before_is_not_a_pass(self):
        empty = self.jellyfin("empty.db")
        self.assertEqual(checks_db.counts(empty, empty), ["no item before the scan: nothing to count"])


class CommandLine(Databases):
    def test_exit_status(self):
        before = self.jellyfin("before.db", item("A"))
        after = self.jellyfin("after.db")
        with redirect_stdout(io.StringIO()) as out:
            self.assertEqual(checks_db.main(["counts", "--before", str(before), "--after", str(before)]), 0)
            self.assertEqual(checks_db.main(["counts", "--before", str(before), "--after", str(after)]), 1)
            self.assertEqual(checks_db.main(["counts", "--before", str(self.tmp / "missing.db"), "--after",
                                             str(after)]), 2)
        self.assertIn("cannot read or write a database", out.getvalue())


class Allowlist(unittest.TestCase):
    SCHEMA = allowlist.schema_columns(SCHEMA)

    def test_every_entry_holds_against_jellyfin_s_schema(self):
        self.assertEqual(allowlist.problems(allowlist.RULES, self.SCHEMA), [])
        with redirect_stdout(io.StringIO()):
            self.assertEqual(allowlist.main(["check", "--schema", str(SCHEMA)]), 0)

    def test_an_entry_that_cannot_apply_or_says_no_why_is_named(self):
        bad = (
            Rule("one", ("scan",), "NoSuchTable", "added", "gate", "why"),
            Rule("Two two", ("later",), "BaseItems", "moved", "guess", " ", columns=("Nope",), where=(("Also", "("),)),
            Rule("three", ("scan",), "BaseItems", "added", "gate", "why", columns=("Name",)),
            Rule("four", ("scan",), "BaseItems", "changed", "gate", "why", columns=("Name",), within=0.05),
            Rule("five", ("scan",), "TrickplayInfos", "changed", "gate", "why", columns=("Bandwidth",), within=5),
            Rule("six", ("scan",), "BaseItems", "changed", "gate", "why", columns=("Width",), keys=("X",)),
            Rule("seven", ("scan",), "BaseItems", "changed", "allowlisted", "a verdict", columns=("Name",)),
            Rule("one", ("scan",), "Users", "added", "gate", "why"),
            Rule("eight", ("scan",), "Users", "added", "gate", "why", where=((allowlist.ITEM_TYPE, ".*"),)),
            Rule("nine", ("scan",), "BaseItemImageInfos", "changed", "gate", "why", columns=(allowlist.ITEM_TYPE,)),
            Rule("ten", ("scan",), "Users", "added", "jellyfin-alone", "Jellyfin does it"),
            Rule("eleven", ("scan",), "Users", "added", "jellyfin-alone", "Jellyfin 13.0 alone does it", becomes="("),
        )
        found = allowlist.problems(bad + bad[:1], self.SCHEMA)
        for wanted in ("entry 1 (one: NoSuchTable added): no table NoSuchTable in Jellyfin's schema",
                       "entry 2 (Two two: BaseItems moved): a name is lower-case letters, digits and dashes",
                       "entry 2 (Two two: BaseItems moved): windows ['later'] not among",
                       "entry 2 (Two two: BaseItems moved): change 'moved' not among",
                       "entry 2 (Two two: BaseItems moved): kind 'guess' not among",
                       "entry 2 (Two two: BaseItems moved): no cause",
                       "entry 2 (Two two: BaseItems moved): no column Nope in BaseItems",
                       "entry 2 (Two two: BaseItems moved): no column Also in BaseItems",
                       "entry 2 (Two two: BaseItems moved): where Also:",
                       "entry 3 (three: BaseItems added): a changed row needs the columns it may change",
                       "entry 4 (four: BaseItems changed): a tolerance on a column that is not a number: Name",
                       "entry 5 (five: TrickplayInfos changed): a tolerance is a fraction between 0 and 1, not 5",
                       "entry 6 (six: BaseItems changed): JSON keys only on text columns",
                       "entry 7 (seven: BaseItems changed): an allowlisted entry's cause names the issue that decided it",
                       "entry 9 (eight: Users added): no column ItemType in Users",   # a table with no ItemId
                       "entry 10 (nine: BaseItemImageInfos changed): no column ItemType in BaseItemImageInfos",
                       "entry 11 (ten: Users added): a jellyfin-alone entry's cause names the run of Jellyfin <version> alone",
                       "entry 12 (eleven: Users added): becomes only on a changed row",
                       "entry 12 (eleven: Users added): becomes: ",
                       "an entry is listed twice",
                       "two entries share a name"):
            self.assertTrue(any(problem.startswith(wanted) for problem in found), (wanted, found))
        with tempfile.NamedTemporaryFile("w", suffix=".sql") as broken, redirect_stdout(io.StringIO()) as out:
            broken.write("CREATE TABLE")
            broken.flush()
            self.assertEqual(allowlist.main(["check", "--schema", broken.name]), 1)
        self.assertIn("cannot read Jellyfin's schema", out.getvalue())

    def test_a_tolerance_covers_a_number_within_it_only(self):
        rule = Rule("bandwidth", ("scan",), "TrickplayInfos", "changed", "allowlisted", "why (gl-1)", columns=("Bandwidth",),
                    within=0.05)
        self.assertTrue(rule.covers("Bandwidth", 1000, 1050))
        self.assertTrue(rule.covers("Bandwidth", 1000, 950))
        self.assertFalse(rule.covers("Bandwidth", 1000, 1051))
        self.assertFalse(rule.covers("Bandwidth", "1000", "1001"))
        self.assertFalse(rule.covers("Width", 1000, 1000))


if __name__ == "__main__":
    unittest.main()
