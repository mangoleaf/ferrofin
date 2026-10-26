"""gates/session.py: a session's directory name and the record of its gates (stdlib unittest)."""

import io
import json
import os
import sys
import tempfile
import unittest
from datetime import datetime
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import session  # noqa: E402

WHEN = datetime(2026, 10, 24, 18, 5)


class SessionTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.runs = os.path.join(self.tmp.name, "runs")

    def tearDown(self):
        self.tmp.cleanup()

    def new(self, branch="feat/gates", gates=("verify", "bench")):
        return session.new(self.runs, list(gates), branch, "c0ffee", False, "ferrofin:test", "sha256:t",
                           "jf@sha256:x", when=WHEN)

    def record(self, s):
        return json.loads(Path(s, "session.json").read_text())

    def test_named_for_the_branch_and_the_minute_and_a_rerun_gets_a_counter(self):
        names = [os.path.basename(self.new()) for _ in range(3)]
        self.assertEqual(names, ["feat-gates-20261024-1805", "feat-gates-20261024-1805-run2",
                                 "feat-gates-20261024-1805-run3"])
        self.assertEqual(os.path.basename(self.new(branch="release/v1.4.0")), "release-v1.4.0-20261024-1805")
        self.assertEqual(os.path.basename(self.new(branch="../x y")), "x-y-20261024-1805")

    def test_every_gate_starts_pending_and_the_record_holds_what_was_run(self):
        r = self.record(self.new())
        self.assertEqual((r["branch"], r["commit"], r["dirty"], r["image"], r["oracle"]),
                         ("feat/gates", "c0ffee", False, {"ref": "ferrofin:test", "id": "sha256:t"},
                          {"image": "jf@sha256:x"}))
        self.assertEqual({g: e["status"] for g, e in r["gates"].items()}, {"verify": "pending", "bench": "pending"})
        self.assertNotIn("previous", r)

    def test_the_oracle_records_the_release_the_branch_pins_and_a_newer_one(self):
        note = "measured against Jellyfin v12.2, the release this branch pins; v12.3 is out"
        s = session.new(self.runs, ["bench"], "release/v1.4.0", "c0ffee", False, "ferrofin:test", "sha256:t",
                        "jf@sha256:x", "ferrofin:old", "sha256:o", when=WHEN,
                        oracle_pin={"tag": "v12.2", "image": "jf@sha256:x", "note": note})
        self.assertEqual(self.record(s)["oracle"], {"image": "jf@sha256:x", "tag": "v12.2", "note": note})
        # what gates/run passes: the JSON gates/bench/oracle.py check printed
        with patch("sys.stdout", new_callable=io.StringIO) as printed:
            session.main(["new", self.runs, "--gates", "bench", "--branch", "b", "--commit", "c", "--dirty", "false",
                          "--image", "i", "--image-id", "sha256:i", "--oracle-image", "jf@sha256:x",
                          "--oracle-pin", '{"tag": "v12.2", "image": "jf@sha256:x"}'])
        self.assertEqual(self.record(printed.getvalue().strip())["oracle"], {"image": "jf@sha256:x", "tag": "v12.2"})

    def test_a_gate_records_its_start_end_and_exit(self):
        s = self.new()
        session.gate(s, "verify", "running")
        session.gate(s, "verify", "failed", 3)
        entry = self.record(s)["gates"]["verify"]
        self.assertEqual((entry["status"], entry["exit"]), ("failed", 3))
        self.assertTrue(entry["started"] and entry["ended"])

    def test_the_end_fails_a_running_gate_as_interrupted_and_skips_the_pending(self):
        s = self.new()
        session.gate(s, "verify", "running")
        session.end(s)
        r = self.record(s)
        self.assertEqual({g: (e["status"], e.get("note")) for g, e in r["gates"].items()},
                         {"verify": ("failed", "interrupted"), "bench": ("skipped", "not reached")})
        self.assertIsNotNone(r["ended"])

    def test_unknown_gates_and_statuses_are_refused(self):
        with self.assertRaisesRegex(ValueError, "unknown gate nope"):
            self.new(gates=("verify", "nope"))
        s = self.new()
        with self.assertRaisesRegex(ValueError, "unknown status done"):
            session.gate(s, "verify", "done")
        with self.assertRaisesRegex(ValueError, "parity is not a gate of this session"):
            session.gate(s, "parity", "running")


if __name__ == "__main__":
    unittest.main()
