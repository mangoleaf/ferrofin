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

# The stub numbers what the stage creates from 100: the playlist, the one to delete, the collection, the one to delete.
PLAYLIST, DOOMED_PLAYLIST, COLLECTION, DOOMED_COLLECTION = (f"{n:032x}" for n in range(100, 104))


class UsageStage(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)
        environ = mock.patch.dict(os.environ)   # the probed account is the copy's default, whatever the shell says
        environ.start()
        self.addCleanup(environ.stop)
        os.environ.pop("ADOPTION_USER", None)
        os.environ["ROUNDTRIP_SCAN_TIMEOUT"] = "10"
        pause = mock.patch.object(usage.api.time, "sleep")   # settle's polls 2 s apart: counted, not waited for
        pause.start()
        self.addCleanup(pause.stop)
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
        self.assertEqual(sum("played=1" in line for line in user_data), 5)   # 1 + 3 marked, 1 played by roundtrip-user
        self.assertEqual(sum("fav=1" in line for line in user_data), 2)
        self.assertIn(f"userdata synthetic-admin {MOVIES[4]} played=0 fav=0 resume=300000000", user_data)
        self.assertIn(f"userdata roundtrip-user {MOVIES[0]} played=1 fav=0 resume=0", user_data)
        self.assertEqual(user_data, sorted(user_data))
        for wanted in (f"userplays synthetic-admin {MOVIES[6]} count=0 likes=true",
                       "badlogin roundtrip-user 401", "login roundtrip-user 200 token=yes",
                       "login roundtrip-passwordless 200 token=yes", "device roundtrip-roundtrip-user name=Round trip device",
                       "apikeys Round trip,Synthetic adoption", "libraries Round trip library,Synthetic Movies",
                       "collection Playlist 2 Round trip playlist renamed",
                       "members Playlist Round trip playlist renamed = Movie 3,Movie 2",
                       "collection BoxSet 2 Round trip collection", "members BoxSet Round trip collection = Movie 4,Movie 5",
                       "image BoxSet Round trip collection Primary", "image Playlist Round trip playlist renamed -",
                       "item Round trip edited movie lock=1 fields=Name,Overview overview=37"):
            self.assertIn(wanted, lines)
        self.assertTrue(any(line.startswith("user roundtrip-passwordless admin=0 disabled=0 hidden=1 folders=1 ")
                            and line.endswith(" sessions=2 audio=deu subs=Always autoplay=0") for line in lines))
        self.assertTrue(any(line.startswith("prefs synthetic-admin DateCreated Descending ")
                            and "roundtrip=on" in line for line in lines))
        self.assertEqual([line.split()[1] for line in lines if line.startswith("config ")], ["system", "encoding", "network"])
        self.assertFalse(any("deleted" in line for line in lines))
        actions = (self.tmp / "oracle.txt.actions").read_text().splitlines()
        self.assertIn("give the collection a primary image  204", actions)
        self.assertEqual(actions[-1], "refresh the library again  completed")

    def test_edits_and_deletes_wait_for_a_scan_and_delete_only_the_items_made_for_it(self):
        stub = Stub()
        self.assertEqual(self.run_stage(stub), (0, ""))
        calls = stub.calls
        refreshes = [i for i, call in enumerate(calls) if call == ("POST", "/Library/Refresh")]
        deletes = [(i, path) for i, (method, path) in enumerate(calls) if method == "DELETE" and path.startswith("/Items/")]
        self.assertEqual([path for _, path in deletes], [f"/Items/{DOOMED_PLAYLIST}", f"/Items/{DOOMED_COLLECTION}"])
        later = [calls.index(("POST", f"/Playlists/{PLAYLIST}")), calls.index(("POST", f"/Items/{COLLECTION}/Images/Primary")),
                 calls.index(("POST", f"/Items/{MOVIES[5]}")), *(i for i, _ in deletes)]
        # every creation and membership change, a completed scan, the rename, edit, image and deletes, a second scan
        self.assertEqual(len(refreshes), 2)
        self.assertLess(calls.index(("DELETE", f"/Collections/{COLLECTION}/Items")), refreshes[0])
        self.assertTrue(all(refreshes[0] < i < refreshes[1] for i in later), (refreshes, later))
        # each scan is followed by a settle: the scan's own completed poll, then two idle polls
        for start, end in ((refreshes[0], min(later)), (refreshes[1], len(calls))):
            self.assertGreaterEqual(calls[start:end].count(("GET", "/ScheduledTasks")), 3, calls[start:end])
        # the deleted items are asked for directly, and answer 404
        self.assertIn(("GET", f"/Items/{DOOMED_PLAYLIST}"), calls[refreshes[1]:])

    def test_a_failed_action_is_named_and_no_oracle_is_written(self):
        status, err = self.run_stage(Stub(fail="/Playlists"))
        self.assertEqual(status, 1)
        self.assertEqual(err.strip(), "usage: create the playlist Round trip playlist: HTTP 500")
        self.assertFalse(self.oracle.exists())
        actions = (self.tmp / "oracle.txt.actions").read_text().splitlines()
        self.assertEqual(actions[-1], "create the playlist Round trip playlist  500")
        self.assertTrue(all(line.endswith(("  200", "  204")) for line in actions[:-1]), actions)

    def test_a_write_the_server_accepted_but_lost_fails_the_stage(self):
        status, err = self.run_stage(Stub(drop="/UserFavoriteItems"))
        self.assertEqual(status, 1)
        self.assertEqual(err.strip(), f"usage: favorite Movie 1: not in Ferrofin's answers (wanted userdata "
                                      f"synthetic-admin {MOVIES[0]} with fav=1)")

    def test_each_lost_write_fails_the_stage_naming_its_action(self):
        restricted = f"{1:032x}"   # the stub numbers accounts in creation order: roundtrip-passwordless first
        for drop, action in (
                (f"/UserPlayedItems/{MOVIES[1]}", "mark Movie 2 played"),
                (f"/UserItems/{MOVIES[4]}/UserData", "set a resume position on Movie 5"),
                ("/Devices/Options", "name the device"),
                (f"/UserItems/{MOVIES[6]}/Rating", "like Movie 7"),
                ("/Sessions/Playing/Stopped", "play Movie 1 to its end"),
                ("/Auth/Keys", "create an API key"),
                (f"/Users/{restricted}/Policy", "narrow roundtrip-passwordless's policy"),
                ("/Users/Configuration", "change roundtrip-passwordless's settings"),
                ("/DisplayPreferences/usersettings", "change the display preferences"),
                ("POST /Library/VirtualFolders", "add a library"),
                ("DELETE /Library/VirtualFolders", "remove the library Synthetic Photos"),
                (f"POST /Playlists/{PLAYLIST}/Items$", "add to the playlist"),
                (f"DELETE /Playlists/{PLAYLIST}/Items", "remove from the playlist"),
                (f"/Playlists/{PLAYLIST}/Items/", "move in the playlist"),
                (f"/Playlists/{PLAYLIST}$", "rename the playlist"),
                ("POST /Collections/", "add to the collection"),
                ("DELETE /Collections/", "remove from the collection"),
                (f"/Items/{COLLECTION}/Images", "give the collection a primary image"),
                (f"/Items/{MOVIES[5]}", "edit and lock Movie 6"),
                (f"/Items/{DOOMED_PLAYLIST}", "delete the playlist Round trip deleted playlist"),
                (f"/Items/{DOOMED_COLLECTION}", "delete the collection Round trip deleted collection")):
            with self.subTest(drop=drop):
                status, err = self.run_stage(Stub(drop=drop))
                self.assertEqual(status, 1)
                self.assertTrue(err.startswith(f"usage: {action}: "), err)

    def test_a_failed_scan_and_a_request_with_no_answer_are_named_and_recorded(self):
        class FailedScan(Stub):
            def get(self, path, query):
                status, result = super().get(path, query)
                if path == "/ScheduledTasks" and self.refreshes:
                    result[0]["LastExecutionResult"]["Status"] = "Failed"
                return status, result

        status, err = self.run_stage(FailedScan())
        self.assertEqual((status, err.strip()), (1, "usage: refresh the library: library scan did not complete successfully"))
        self.assertEqual((self.tmp / "oracle.txt.actions").read_text().splitlines()[-1], "refresh the library  failed")

        class Gone(Stub):
            def answer(self, method, path, query, body, auth=""):
                if path == "/Auth/Keys":
                    raise ConnectionResetError   # the handler drops the connection: no HTTP answer
                return super().answer(method, path, query, body, auth)

        class GoneMidScan(Stub):
            def answer(self, method, path, query, body, auth=""):
                if path == "/ScheduledTasks" and self.refreshes:
                    raise ConnectionResetError   # the server drops the scan's poll
                return super().answer(method, path, query, body, auth)

        status, err = self.run_stage(GoneMidScan())
        self.assertEqual(status, 1)
        self.assertTrue(err.strip().splitlines()[-1].startswith(
            "usage: refresh the library: GET /ScheduledTasks: no answer"), err)
        self.assertEqual((self.tmp / "oracle.txt.actions").read_text().splitlines()[-1], "refresh the library  failed")

        status, err = self.run_stage(Gone())
        self.assertEqual(status, 1)
        # the in-process server prints the dropped connection's traceback first; the stage's line is last
        self.assertTrue(err.strip().splitlines()[-1].startswith("usage: create an API key: POST /Auth/Keys: no answer"), err)
        self.assertEqual((self.tmp / "oracle.txt.actions").read_text().splitlines()[-1], "create an API key  no answer")

    def test_a_created_item_without_an_id_is_named(self):
        class NoId(Stub):
            def post(self, path, query, body, auth):
                status, result = super().post(path, query, body, auth)
                return (200, {}) if path == "/Collections" else (status, result)

        status, err = self.run_stage(NoId())
        self.assertEqual((status, err.strip()), (1, "usage: create the collection Round trip collection: no Id in the answer"))


class Png(unittest.TestCase):
    def test_png_is_a_valid_image_of_the_size_asked(self):
        data = usage.png(3, 2, (1, 2, 3))
        self.assertEqual(data[:8], b"\x89PNG\r\n\x1a\n")
        self.assertEqual((int.from_bytes(data[16:20], "big"), int.from_bytes(data[20:24], "big")), (3, 2))
        try:
            from PIL import Image
        except ImportError:
            self.skipTest("Pillow is not installed")
        with Image.open(io.BytesIO(data)) as image:
            self.assertEqual((image.size, image.convert("RGB").getpixel((2, 1))), ((3, 2), (1, 2, 3)))


if __name__ == "__main__":
    unittest.main()
