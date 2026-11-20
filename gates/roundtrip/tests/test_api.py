"""api.Server.settle against a stub whose scheduled tasks run for a while: it waits for two idle polls in a row,
and gives up with a reason once its time is out."""

import shutil
import sys
import tempfile
import threading
import unittest
from pathlib import Path

sys.path.append(str(Path(__file__).resolve().parent.parent))
sys.path.append(str(Path(__file__).resolve().parent))
import api  # noqa: E402
from fake_server import copy, server  # noqa: E402


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
