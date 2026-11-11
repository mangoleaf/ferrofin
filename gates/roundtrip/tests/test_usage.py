"""usage.py and answers.py against a stub server on a throwaway copy (no docker, no Ferrofin): the actions
reach the server, the closing scan is waited for, the oracle holds every kind of answer the contract names,
and a failed action is named. smoke.sh runs for real against the stub (curl, jq and sqlite3)."""

from contextlib import redirect_stderr
import io
import os
import shutil
import sys
import tempfile
import threading
import unittest
from unittest import mock
from pathlib import Path

sys.path.append(str(Path(__file__).resolve().parent.parent))
sys.path.append(str(Path(__file__).resolve().parent))
import usage  # noqa: E402
from fake_server import MOVIES, Stub, copy, server  # noqa: E402

class UsageStage(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)
        environ = mock.patch.dict(os.environ)   # the probed account is the copy's default, whatever the shell says
        environ.start()
        self.addCleanup(environ.stop)
        os.environ.pop("ADOPTION_USER", None)
        os.environ["ROUNDTRIP_SCAN_TIMEOUT"] = "10"
        self.data = copy(self.tmp / "copy")
        self.oracle = self.tmp / "oracle.txt"

    def run_stage(self, stub):
        httpd = server(stub)
        threading.Thread(target=httpd.serve_forever, daemon=True).start()
        self.addCleanup(httpd.server_close)
        self.addCleanup(httpd.shutdown)
        err = io.StringIO()
        with redirect_stderr(err):
            status = usage.main(["--base-url", f"http://127.0.0.1:{httpd.server_port}", "--data-dir", str(self.data),
                                 "--oracle-out", str(self.oracle)])
        return status, err.getvalue()

    def test_the_actions_then_a_completed_scan_then_the_oracle(self):
        stub = Stub()
        status, err = self.run_stage(stub)
        self.assertEqual((status, err), (0, ""))
        posts = [path for method, path in stub.calls if method == "POST"]
        self.assertEqual([p.split("/")[1] for p in posts[:3]], ["UserPlayedItems"] * 3)
        self.assertNotIn(f"/UserPlayedItems/{MOVIES[0]}", posts)   # already played: not one of the three
        self.assertLess(posts.index("/Users/New"), posts.index("/Library/Refresh"))
        lines = self.oracle.read_text().splitlines()
        self.assertTrue(any(line.endswith("/System/Info/Public") for line in lines), lines[:3])
        user_data = [line for line in lines if line.startswith("userdata ")]
        self.assertEqual(sum("played=1" in line for line in user_data), 4)
        self.assertEqual(sum("fav=1" in line for line in user_data), 2)
        self.assertIn(f"userdata synthetic-admin {MOVIES[4]} played=0 fav=0 resume=300000000", user_data)
        self.assertEqual(user_data, sorted(user_data))
        self.assertIn("login roundtrip-user 200 token=yes", lines)
        self.assertIn("collection Playlist 2 Round trip playlist", lines)
        self.assertIn("collection BoxSet 2 Round trip collection", lines)

    def test_a_failed_action_is_named_and_no_oracle_is_written(self):
        status, err = self.run_stage(Stub(fail="/Playlists"))
        self.assertEqual(status, 1)
        self.assertEqual(err.strip(), "usage: create a playlist: HTTP 500")
        self.assertFalse(self.oracle.exists())

    def test_a_write_the_server_accepted_but_lost_fails_the_stage(self):
        status, err = self.run_stage(Stub(drop="/UserFavoriteItems"))
        self.assertEqual(status, 1)
        self.assertEqual(err.strip(), f"usage: favorite Movie 1: not in Ferrofin's answers (wanted userdata "
                                      f"synthetic-admin {MOVIES[0]} with fav=1)")


if __name__ == "__main__":
    unittest.main()
