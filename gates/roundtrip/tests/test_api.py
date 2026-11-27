"""api.Server.settle against a stub whose scheduled tasks run for a while: it waits for two idle polls in a row,
and gives up with a reason once its time is out. api.Server.refresh_playlists against the fake server: it waits until
each playlist's DateLastRefreshed moved, and gives up with a reason once its time is out."""

from contextlib import closing
import shutil
import sqlite3
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock
from pathlib import Path

sys.path.append(str(Path(__file__).resolve().parent.parent))
sys.path.append(str(Path(__file__).resolve().parent))
import api  # noqa: E402
from fake_server import REFRESH_SECONDS, SHARED, Stub, copy, server  # noqa: E402


class Tasks:
    """/ScheduledTasks answers: the given states in turn, then the last one for ever."""

    def __init__(self, *states):
        self.states, self.polls = list(states), 0

    def answer(self, method, path, query, body, auth=""):
        state = self.states[min(self.polls, len(self.states) - 1)]
        self.polls += 1
        return 200, [{"Key": "RefreshLibrary", "State": "Idle"}, {"Key": "Startup", "State": state}]


class Settle(unittest.TestCase):
    def server(self, tasks):
        tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, tmp)
        httpd = server(tasks)
        threading.Thread(target=httpd.serve_forever, daemon=True).start()
        self.addCleanup(httpd.server_close)
        self.addCleanup(httpd.shutdown)
        return api.Server(f"http://127.0.0.1:{httpd.server_port}", copy(tmp / "copy"))

    def test_waits_for_a_task_that_starts_after_the_first_idle_poll(self):
        tasks = Tasks("Idle", "Running", "Running", "Idle")
        self.server(tasks).settle(timeout=10, interval=0.01)
        self.assertEqual(tasks.polls, 5)   # idle, two running, then idle twice

    def test_one_idle_poll_is_not_enough(self):
        tasks = Tasks("Idle")
        self.server(tasks).settle(timeout=10, interval=0.01)
        self.assertEqual(tasks.polls, 2)

    def test_a_task_that_never_finishes_is_a_reason(self):
        tasks = Tasks("Running")
        with self.assertRaisesRegex(api.CheckError, "a scheduled task was still running after 0 s"):
            self.server(tasks).settle(timeout=0.05, interval=0.01)
        self.assertGreater(tasks.polls, 1)   # it waited, not gave up at the first look


class RefreshPlaylists(unittest.TestCase):
    def server(self, stub, data):
        httpd = server(stub)
        threading.Thread(target=httpd.serve_forever, daemon=True).start()
        self.addCleanup(httpd.server_close)
        self.addCleanup(httpd.shutdown)
        return api.Server(f"http://127.0.0.1:{httpd.server_port}", data)

    def copy(self):
        tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, tmp)
        return copy(tmp / "copy")

    def test_each_playlist_is_refreshed_in_full_and_waited_for(self):
        data = self.copy()
        stub = Stub(db=api.database(data))
        started = time.monotonic()
        self.server(stub, data).refresh_playlists(api.database(data), timeout=10, interval=0.01)
        self.assertIn(("POST", f"/Items/{SHARED}/Refresh"), stub.calls)
        self.assertGreaterEqual(time.monotonic() - started, REFRESH_SECONDS)   # the queued refresh was waited for

    def test_a_refresh_that_never_lands_is_a_reason(self):
        data = self.copy()
        with self.assertRaisesRegex(api.CheckError, "1 of 1 playlists were not refreshed after 0 s"):
            self.server(Stub(), data).refresh_playlists(api.database(data), timeout=0.05, interval=0.01)

    def test_a_refresh_of_another_mode_is_never_waited_out(self):
        data = self.copy()
        server = self.server(Stub(db=api.database(data)), data)
        original = api.Server.request

        def default_mode(self, path, method="GET", **options):
            return original(self, path.replace("FullRefresh", "Default"), method, **options)

        with mock.patch.object(api.Server, "request", default_mode):
            with self.assertRaisesRegex(api.CheckError, "were not refreshed"):
                server.refresh_playlists(api.database(data), timeout=REFRESH_SECONDS * 3, interval=0.01)

    def test_a_copy_without_playlists_is_a_reason_not_a_pass(self):
        data = self.copy()
        with closing(sqlite3.connect(api.database(data))) as db:
            db.execute("DELETE FROM BaseItems WHERE Type = ?", (api.PLAYLIST,))
            db.commit()
        with self.assertRaisesRegex(api.CheckError, "holds no playlist to refresh"):
            self.server(Stub(), data).refresh_playlists(api.database(data), timeout=1, interval=0.01)


class Send(unittest.TestCase):
    def test_a_binary_body_is_no_json_but_its_status_counts(self):
        from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

        class Image(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_GET(self):
                body = b"\xff\xd8\xff\xe0 a jpeg"
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, tmp)
        httpd = ThreadingHTTPServer(("127.0.0.1", 0), Image)
        threading.Thread(target=httpd.serve_forever, daemon=True).start()
        self.addCleanup(httpd.server_close)
        self.addCleanup(httpd.shutdown)
        server = api.Server(f"http://127.0.0.1:{httpd.server_port}", copy(tmp / "copy"))
        self.assertEqual(server.send("GET", "/Items/x/Images/Primary/0"), (200, None))


if __name__ == "__main__":
    unittest.main()
