"""livetv.py: the fixture both servers read, and how Jellyfin's answers are judged against Ferrofin's."""

import datetime
import re
import sys
import tempfile
import unittest
from pathlib import Path

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


if __name__ == "__main__":
    unittest.main()
