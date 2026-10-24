"""Regression tests using invented library metadata and a local HTTP fixture."""

import contextlib
import copy
import importlib.util
import io
import json
from pathlib import Path
import sqlite3
import struct
import tempfile
import threading
import unittest
import zlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest.mock import patch
from urllib.parse import parse_qs, urlsplit

spec = importlib.util.spec_from_file_location("metadata", Path(__file__).parents[1] / "metadata.py")
metadata = importlib.util.module_from_spec(spec)
spec.loader.exec_module(metadata)
MOVIE = "a" * 32
EPISODE = "b" * 32


def png():
    def chunk(kind, data):
        return struct.pack("!I", len(data)) + kind + data + struct.pack("!I", zlib.crc32(kind + data))
    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack("!2I5B", 1, 1, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(b"\0\xff\0\0")) + chunk(b"IEND", b""))


class MetadataTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.db = Path(self.tmp.name) / "library.db"
        self.baseline = Path(self.tmp.name) / "baseline.json"
        with contextlib.closing(sqlite3.connect(self.db)) as db, db:
            db.executescript('''
                CREATE TABLE BaseItems (Id TEXT, Type TEXT, Overview TEXT,
                  OriginalTitle TEXT, Tagline TEXT, CommunityRating REAL,
                  CriticRating REAL, OfficialRating TEXT, ProductionYear INTEGER,
                  PremiereDate TEXT, Genres TEXT, Studios TEXT, Tags TEXT,
                  ProductionLocations TEXT);
                CREATE TABLE BaseItemProviders (ItemId TEXT, ProviderId TEXT, ProviderValue TEXT);
                CREATE TABLE BaseItemImageInfos (ItemId TEXT, ImageType INTEGER, Path TEXT);
                CREATE TABLE Peoples (Id TEXT, Name TEXT);
                CREATE TABLE PeopleBaseItemMap (ItemId TEXT, PeopleId TEXT, Role TEXT);
                CREATE TABLE ApiKeys (AccessToken TEXT, DateCreated TEXT);
                CREATE TABLE Users (Id TEXT);
                CREATE TABLE Permissions (UserId TEXT, Kind INTEGER, Value INTEGER);
                INSERT INTO Users VALUES ('dddddddd-dddd-dddd-dddd-dddddddddddd');
                INSERT INTO Permissions VALUES ('dddddddd-dddd-dddd-dddd-dddddddddddd', 0, 1);
                INSERT INTO ApiKeys VALUES ('secret-token', '2026-01-01');
            ''')
            for key, kind in [(MOVIE, "Movie"), (EPISODE, "Episode")]:
                db.execute("INSERT INTO BaseItems VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)", (
                    str(metadata.uuid.UUID(key)).upper(), "MediaBrowser.Controller.Entities." + kind,
                    "Private description", "Private title", "Private tagline", 7.5, 80,
                    "PG", 2001, "2001-01-01", "Drama", "Studio", "Tag", "Location"))
                db.execute("INSERT INTO BaseItemProviders VALUES (?, 'Tmdb', '123456789')", (key,))
                db.execute("INSERT INTO BaseItemImageInfos VALUES (?, 0, '/private/poster.jpg')", (key,))
                db.execute("INSERT INTO PeopleBaseItemMap VALUES (?, 'person', 'Actor')", (key,))
            db.execute("INSERT INTO Peoples VALUES ('person', 'Private actor')")
        self.expected = metadata.snapshot(self.db)
        self.baseline.write_text(json.dumps(self.expected))

    def sql(self, sql):
        with contextlib.closing(sqlite3.connect(self.db)) as db, db:
            db.executescript(sql)

    def test_baseline_records_presence_without_library_content(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(metadata.main(["snapshot", str(self.db), str(self.baseline)]), 0)
        text = self.baseline.read_text()
        for secret in ["Private", "123456789", "/private", "secret-token"]:
            self.assertNotIn(secret, text)
        self.assertEqual(self.expected, json.loads(text))
        self.assertEqual(self.baseline.stat().st_mode & 0o777, 0o600)
        self.assertFalse(metadata.compare(self.expected, metadata.snapshot(self.db), "database"))

    def test_empty_and_unenriched_fixtures_fail(self):
        for change in ["DELETE FROM BaseItems", "UPDATE BaseItems SET Overview = NULL",
                       "DELETE FROM BaseItemProviders"]:
            with self.subTest(change=change):
                data = copy.deepcopy(self.expected)
                if "DELETE FROM BaseItems" in change:
                    data["items"] = {}
                elif "Overview" in change:
                    data["items"][MOVIE]["fields"].remove("Overview")
                else:
                    data["items"][MOVIE]["providers"] = []
                with self.assertRaises(metadata.CheckError):
                    metadata.validate_baseline(data)

    def test_every_populated_field_is_checked_per_item(self):
        for field in metadata.FIELDS:
            with self.subTest(field=field):
                after = copy.deepcopy(self.expected)
                after["items"][EPISODE]["fields"].remove(field)
                failures = metadata.compare(self.expected, after, "database")
                self.assertEqual(failures, {f"database missing {field}": 1})

    def test_real_database_metadata_loss_is_detected(self):
        self.sql("UPDATE BaseItems SET Overview = ''; DELETE FROM BaseItemProviders; "
                 "DELETE FROM BaseItemImageInfos; DELETE FROM PeopleBaseItemMap")
        failures = metadata.compare(self.expected, metadata.snapshot(self.db), "database")
        self.assertEqual(failures, {"database missing Overview": 2,
                                  "database missing ProviderIds": 2,
                                  "database missing Images": 2, "database missing People": 2})

    def test_total_counts_cannot_hide_one_items_loss(self):
        self.sql(f"UPDATE BaseItems SET Id = '{'c' * 32}' WHERE Type LIKE '%Movie'")
        self.assertEqual(metadata.compare(self.expected, metadata.snapshot(self.db), "database"),
                         {"database missing item": 1})

    def test_optional_fields_and_legitimate_provider_updates_are_allowed(self):
        self.sql("UPDATE BaseItems SET Overview = 'Updated description', CommunityRating = 8.0; "
                 "UPDATE BaseItemProviders SET ProviderValue = '42'")
        self.assertFalse(metadata.compare(self.expected, metadata.snapshot(self.db), "database"))
        self.sql("UPDATE BaseItems SET Tagline = NULL, CommunityRating = 0")
        before = metadata.snapshot(self.db)
        self.assertNotIn("Tagline", before["items"][MOVIE]["fields"])
        self.assertNotIn("CommunityRating", before["items"][MOVIE]["fields"])
        self.assertFalse(metadata.compare(before, metadata.snapshot(self.db), "database"))

    def test_missing_and_invalid_database_fail_without_creating_a_file(self):
        missing = Path(self.tmp.name) / "missing.db"
        with contextlib.redirect_stdout(io.StringIO()) as out:
            result = metadata.main(["snapshot", str(missing), str(self.baseline)])
        self.assertEqual(result, 1)
        self.assertFalse(missing.exists())
        self.assertNotIn(str(missing), out.getvalue())
        missing.write_text("not sqlite")
        with self.assertRaises(sqlite3.DatabaseError):
            metadata.snapshot(missing)

    def test_invalid_baseline_fails_without_a_traceback(self):
        for value in ("not JSON", "[]", "{}"):
            self.baseline.write_text(value)
            with contextlib.redirect_stdout(io.StringIO()) as out:
                result = metadata.main(["check", str(self.db), str(self.baseline)])
            self.assertEqual(result, 1)
            self.assertNotIn("Traceback", out.getvalue())

    def test_missing_api_credentials_fail(self):
        self.sql("DELETE FROM ApiKeys")
        with self.assertRaisesRegex(metadata.CheckError, "API key"):
            metadata.Api("http://127.0.0.1", self.db)

    def test_live_wal_metadata_is_seen(self):
        with contextlib.closing(sqlite3.connect(self.db)) as db, db:
            db.execute("PRAGMA journal_mode=WAL")
            db.execute("UPDATE BaseItems SET Overview = NULL")
            db.commit()
            self.assertEqual(metadata.compare(self.expected, metadata.snapshot(self.db), "database"),
                             {"database missing Overview": 2})

    def api_fixture(self, missing_field=None, status=200, scan_status="Completed", stale=False, hide_item=False, missing_item=False, image_body=None):
        owner = self
        self.requests = []
        self.scan_calls = 0

        def item_row(key):
            before = owner.expected["items"][key]
            row = {field: "populated" for field in metadata.FIELDS}
            row.update(Id=key, Type=before["kind"], Taglines=["tagline"],
                       ProviderIds={"Tmdb": "123456789"}, People=[{"Name": "Private actor"}],
                       ImageTags={"Primary": "tag"})
            row.pop(missing_field, None)
            return row

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_POST(self):
                owner.requests.append(self.path)
                self.send_response(204)
                self.end_headers()

            def do_GET(self):
                owner.requests.append(self.path)
                if self.path.startswith("/ScheduledTasks"):
                    owner.scan_calls += 1
                    result = {"EndTimeUtc": "old", "Status": "Completed"}
                    if owner.scan_calls > 2 and not stale:
                        result = {"EndTimeUtc": "new", "Status": scan_status}
                    payload = [{"Key": "RefreshLibrary", "State": "Idle", "LastExecutionResult": result}]
                elif self.path.startswith("/Users/"):
                    if missing_item:
                        self.send_response(404)
                        self.end_headers()
                        return
                    payload = item_row(self.path.rsplit("/", 1)[-1])
                elif "/Images/" in self.path:
                    self.send_response(status)
                    self.send_header("Content-Type", "image/png")
                    self.end_headers()
                    self.wfile.write(png() if image_body is None else image_body)
                    return
                else:
                    query = parse_qs(urlsplit(self.path).query)
                    owner.assertEqual(query["UserId"], ["d" * 32])
                    # Reject accidental use of scalar DTO properties as ItemFields flags.
                    owner.assertEqual(set(query["Fields"][0].split(",")), {
                        "Overview", "OriginalTitle", "Taglines", "Genres", "Studios", "Tags",
                        "ProductionLocations", "ProviderIds", "People"})
                    payload = {"Items": [item_row(key) for key in query["Ids"][0].split(",")
                                         if not ((hide_item or missing_item) and key == MOVIE)]}
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(json.dumps(payload).encode())
        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        return f"http://127.0.0.1:{server.server_port}"

    def run_check(self, mode="check", **kwargs):
        base = self.api_fixture(**kwargs)
        with contextlib.redirect_stdout(io.StringIO()) as out:
            result = metadata.main([mode, str(self.db), str(self.baseline), "--base-url", base])
        return result, out.getvalue()

    def test_database_and_real_http_are_checked(self):
        result, output = self.run_check()
        self.assertEqual((result, output), (0, ""))
        self.assertEqual(sum("/Images/" in path for path in self.requests), 2)
        from PIL import Image
        for format_name in ("PNG", "JPEG", "WEBP"):
            with self.subTest(format=format_name), io.BytesIO() as body:
                with Image.new("RGB", (2, 2), "red") as image:
                    image.save(body, format=format_name)
                self.assertTrue(metadata.decodable_image(body.getvalue()))

    def test_api_loss_fails_even_when_database_is_intact(self):
        result, output = self.run_check(missing_field="Overview")
        self.assertEqual(result, 1)
        self.assertEqual(output, "API missing Overview: 2 item(s)\n")
        for secret in (MOVIE, EPISODE, "Private", "123456789"):
            self.assertNotIn(secret, output)

    def test_http_errors_fail(self):
        result, output = self.run_check(status=500)
        self.assertEqual(result, 1)
        self.assertEqual(output, "metadata HTTP request returned 500\n")

    def test_scan_waits_for_a_new_completed_result(self):
        base = self.api_fixture()
        metadata.Api(base, self.db).scan(10, interval=0)
        self.assertGreaterEqual(self.scan_calls, 3)
        self.assertIn("/Library/Refresh", self.requests)

    def test_failed_or_cancelled_scan_fails(self):
        for status in ("Failed", "Cancelled", "Aborted"):
            with self.subTest(status=status):
                base = self.api_fixture(scan_status=status)
                with self.assertRaisesRegex(metadata.CheckError, "did not complete"):
                    metadata.Api(base, self.db).scan(10, interval=0)

    def test_idle_with_a_stale_result_does_not_count_as_a_scan(self):
        base = self.api_fixture(stale=True)
        with self.assertRaisesRegex(metadata.CheckError, "timed out"):
            metadata.Api(base, self.db).scan(0.01, interval=0)

    def test_hidden_alternate_version_uses_its_detail_endpoint(self):
        result, output = self.run_check(hide_item=True)
        self.assertEqual((result, output), (0, ""))
        self.assertTrue(any(path.startswith("/Users/") for path in self.requests))

    def test_item_missing_from_both_list_and_detail_fails(self):
        result, output = self.run_check(missing_item=True)
        self.assertEqual(result, 1)
        self.assertEqual(output, "API missing item: 1 item(s)\n")

    def test_api_checks_every_batch(self):
        for i in range(110):
            self.expected["items"][f"{i:032x}"] = copy.deepcopy(self.expected["items"][MOVIE])
        base = self.api_fixture()
        actual = metadata.Api(base, self.db).snapshot(self.expected)
        self.assertFalse(metadata.compare(self.expected, actual, "API"))
        self.assertEqual(len(self.requests), 2)

    def test_scan_mode_checks_metadata_after_completion(self):
        with patch.object(metadata.time, "sleep"):
            result, output = self.run_check(mode="scan", missing_field="People")
        self.assertEqual(result, 1)
        self.assertIn("API missing People: 2 item(s)", output)
        self.assertIn("/Library/Refresh", self.requests)

    def test_descriptions_and_ratings_lost_in_database_fail(self):
        for field in ("Overview", "CommunityRating", "CriticRating", "OfficialRating"):
            with self.subTest(field=field):
                with contextlib.closing(sqlite3.connect(self.db)) as db:
                    original = db.execute(f'SELECT "{field}" FROM BaseItems LIMIT 1').fetchone()[0]
                self.sql(f'UPDATE BaseItems SET "{field}" = NULL')
                self.assertEqual(metadata.compare(self.expected, metadata.snapshot(self.db), "database"),
                                 {f"database missing {field}": 2})
                with contextlib.closing(sqlite3.connect(self.db)) as db, db:
                    db.execute(f'UPDATE BaseItems SET "{field}" = ?', (original,))

    def test_descriptions_and_ratings_lost_in_api_fail_after_scan(self):
        for field in ("Overview", "CommunityRating", "CriticRating", "OfficialRating"):
            with self.subTest(field=field), patch.object(metadata.time, "sleep"):
                result, output = self.run_check(mode="scan", missing_field=field)
                self.assertEqual(result, 1)
                self.assertIn(f"API missing {field}: 2 item(s)", output)

    def test_artwork_sample_is_ten_percent_per_kind_and_stable(self):
        items = {}
        for kind, count, offset in (("Movie", 318, 0), ("Series", 126, 1000), ("Episode", 30, 2000)):
            for i in range(count):
                items[f"{i + offset:032x}"] = {"kind": kind, "images": ["Primary", "Backdrop"]}
        api = metadata.Api("http://127.0.0.1", self.db)
        with patch.object(api, "request", return_value=True) as request:
            api.images({"items": items})
            first = request.call_args_list[:]
            request.reset_mock()
            api.images({"items": dict(reversed(list(items.items())))})
            self.assertCountEqual(first, request.call_args_list)
        ids = {call.args[0].split("/")[2] for call in first}
        self.assertEqual(sum(items[key]["kind"] == "Movie" for key in ids), 32)
        self.assertEqual(sum(items[key]["kind"] == "Series" for key in ids), 13)
        self.assertEqual(sum(items[key]["kind"] == "Episode" for key in ids), 1)
        self.assertEqual(len(first), (32 + 13) * 2 + 1)

    def test_artwork_sample_handles_missing_optional_images(self):
        api = metadata.Api("http://127.0.0.1", self.db)
        self.expected["items"][MOVIE]["images"] = ["Backdrop"]
        self.expected["items"][EPISODE]["images"] = []
        with patch.object(api, "request", return_value=True) as request:
            api.images(self.expected)
            request.assert_called_once_with(f"/Items/{MOVIE}/Images/Backdrop/0?maxWidth=200", image=True)

    def test_corrupt_artwork_with_image_content_type_fails(self):
        for body in (b"", b"not actually an image", png()[:33]):
            with self.subTest(body_length=len(body)):
                result, output = self.run_check(image_body=body)
                self.assertEqual(result, 1)
                self.assertIn("image was empty or undecodable", output)
                self.assertNotIn(MOVIE, output)

    def test_image_decoder_unavailable_fails(self):
        with patch.dict("sys.modules", {"PIL": None}):
            with self.assertRaisesRegex(metadata.CheckError, "decoder unavailable: install Pillow"):
                metadata.decodable_image(png())


if __name__ == "__main__":
    unittest.main()
