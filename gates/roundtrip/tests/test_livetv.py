"""livetv.py: the fixture both servers read, and how Jellyfin's answers are judged against Ferrofin's."""

import datetime
import re
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.append(str(Path(__file__).resolve().parent.parent))
import livetv  # noqa: E402


class Fixture(unittest.TestCase):
    def test_the_guide_starts_the_next_day_and_one_programme_is_in_plus_one(self):
        with tempfile.TemporaryDirectory() as made:
            now = datetime.datetime(2026, 11, 21, 23, 30, tzinfo=datetime.timezone.utc)
            livetv.fixture(made, now)
            guide = (Path(made) / "guide.xml").read_text()
            tuner = (Path(made) / "tuner.m3u").read_text()
        starts = re.findall(r'start="(\d{14}) ([+-]\d{4})"', guide)
        self.assertEqual(len(starts), 8)
        self.assertEqual(starts[0], ("20261122180000", "+0000"))
        self.assertIn(("20261123090000", "+0100"), starts, "08:00 UTC, written in +01:00")
        utc = [datetime.datetime.strptime(s + o, "%Y%m%d%H%M%S%z") for s, o in starts]
        self.assertGreater(min(utc), now, "no airing has begun when the run starts")
        self.assertEqual(tuner.count("#EXTINF"), 2)
        self.assertEqual(guide.count("<title>Quiz Night</title>"), 6)


class Differences(unittest.TestCase):
    FERROFIN = {"timers": [{"Id": "a"}], "favourite_channels": ["Test News"], "programmes": ["p1", "p2"]}

    def test_equal_answers_are_no_difference(self):
        self.assertEqual(livetv.differences(self.FERROFIN, dict(self.FERROFIN), "before"), [])

    def test_each_answer_that_differs_is_one_line_naming_when(self):
        jellyfin = {"timers": [], "favourite_channels": ["Test News"], "programmes": ["p1"]}
        lines = livetv.differences(self.FERROFIN, jellyfin, "after Jellyfin's first guide refresh")
        self.assertEqual(len(lines), 2)
        self.assertTrue(all(line.startswith("Live TV after Jellyfin's first guide refresh: ") for line in lines))
        self.assertIn("timers differ", lines[0])
        self.assertIn("programmes differ", lines[1])

    def test_an_answer_jellyfin_lacks_differs(self):
        self.assertEqual(len(livetv.differences({"channels": ["c"]}, {}, "before")), 1)


class Tasks:
    """A server's RefreshGuide task. `busy`: how many more reads the run under way lasts (a tuner or guide save
    queued it); it ends as the next of `ends`. A start runs an idle task for one read. `sneaks`: a queued run takes
    the idle task just before the first start, which is then answered 400."""

    def __init__(self, busy=0, ends=("Completed",), sneaks=False):
        self.busy, self.ends, self.sneaks, self.ended, self.starts = busy, list(ends), sneaks, [], []

    def request(self, path):
        if path == "/ScheduledTasks":
            return [self.task()]
        if self.busy:
            self.busy -= 1
            if not self.busy:
                self.ended.append(self.ends.pop(0))
        return self.task()

    def task(self):
        last = {"Status": self.ended[-1], "EndTimeUtc": str(len(self.ended))} if self.ended else None
        return {"Key": "RefreshGuide", "Id": "guide", "State": "Running" if self.busy else "Idle",
                "LastExecutionResult": last}

    def send(self, method, path):
        assert (method, path) == ("POST", "/ScheduledTasks/Running/guide"), (method, path)
        if self.sneaks:
            self.sneaks, self.busy = False, 2
        self.starts.append("Running" if self.busy else "Idle")
        if self.busy:
            return 400, None
        self.busy = 1
        return 204, None


class RefreshGuide(unittest.TestCase):
    def setUp(self):
        patched = mock.patch.object(livetv, "POLL_SECONDS", 0)
        patched.start()
        self.addCleanup(patched.stop)

    def test_an_idle_task_is_started_and_waited_for(self):
        tasks = Tasks()
        livetv.refresh_guide(tasks, timeout=5)
        self.assertEqual((tasks.starts, tasks.ended), (["Idle"], ["Completed"]))

    def test_the_run_a_save_queued_is_waited_out_and_its_cancelled_end_is_not_the_refreshs(self):
        tasks = Tasks(busy=3, ends=("Cancelled", "Completed"))
        livetv.refresh_guide(tasks, timeout=5)
        self.assertEqual((tasks.starts, tasks.ended), (["Idle"], ["Cancelled", "Completed"]))

    def test_a_start_refused_for_a_run_that_got_in_first_is_asked_again(self):
        tasks = Tasks(ends=("Completed", "Completed"), sneaks=True)
        livetv.refresh_guide(tasks, timeout=5)
        self.assertEqual((tasks.starts, tasks.ended), (["Running", "Idle"], ["Completed", "Completed"]))

    def test_a_refresh_that_does_not_complete_is_a_reason(self):
        with self.assertRaisesRegex(livetv.api.CheckError, "the guide refresh ended Failed"):
            livetv.refresh_guide(Tasks(ends=("Failed",)), timeout=5)

    def test_a_run_that_never_ends_is_a_reason(self):
        with self.assertRaisesRegex(livetv.api.CheckError, "did not finish within 0 s"):
            livetv.refresh_guide(Tasks(busy=10**9), timeout=0.05)


if __name__ == "__main__":
    unittest.main()
