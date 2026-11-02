from contextlib import closing
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest
from contextlib import contextmanager
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from metadata import CheckError
import upgrade


MANIFEST = {"rated_movie": "a" * 32, "private_movie": "b" * 32, "allowed_movie": "c" * 32,
            "users": {"synthetic-admin": "1" * 32, "synthetic-adult": "2" * 32}}
STORED = {"a" * 32: {"Played": True}, "b" * 32: {"PlaybackPositionTicks": upgrade.POSITION},
          "c" * 32: {"IsFavorite": True}}


class FakeApi:
    """Records writes; answers the read-back with what a release that stored them serves."""

    def __init__(self, calls, user="1" * 32, stored=STORED, names=None):
        self.calls, self.user, self.stored = calls, user, stored
        self.names = names if names is not None else ["synthetic-admin", upgrade.CREATED]

    def login(self, name):
        return FakeApi(self.calls, "2" * 32, self.stored, self.names)

    def post(self, path, body=None, **query):
        self.calls.append(("POST", path, body, query))
        return {"Id": ("e" if body and body.get("Name") == upgrade.CREATED else "d") * 32}

    def call(self, method, path, body=None, **query):
        self.calls.append((method, path, body, query))

    def get(self, path, **query):
        if path == "/Users":
            return [{"Name": name} for name in self.names]
        ids = query["ids"].split(",")
        return {"Items": [{"Id": item, "UserData": self.stored.get(item, {})} for item in ids]}


class UserStateTest(unittest.TestCase):
    def test_the_state_is_written_as_the_adult_and_the_users_as_the_admin(self):
        calls = []
        manifest = upgrade.add_user_state(FakeApi(calls), MANIFEST)
        self.assertEqual(manifest["users"][upgrade.CREATED], "e" * 32)
        self.assertNotIn(upgrade.CREATED, MANIFEST["users"])
        paths = [(method, path) for method, path, _, _ in calls]
        self.assertIn(("POST", f"/UserPlayedItems/{'a' * 32}"), paths)
        self.assertIn(("POST", f"/UserItems/{'b' * 32}/UserData"), paths)
        self.assertIn(("POST", f"/UserFavoriteItems/{'c' * 32}"), paths)
        self.assertEqual([body["Name"] for method, path, body, _ in calls if path == "/Users/New"],
                         [upgrade.CREATED, upgrade.DELETED])
        self.assertIn(("DELETE", f"/Users/{'d' * 32}"), paths)
        self.assertEqual([query["userId"] for method, path, _, query in calls if path == "/UserImage"], ["e" * 32])
        self.assertTrue(all(query.get("userId") == "2" * 32 for _, path, _, query in calls
                            if not path.startswith(("/Users", "/UserImage"))))

    def test_a_state_the_start_release_did_not_store_fails_before_the_upgrade(self):
        with self.assertRaisesRegex(CheckError, "the resume position"):
            upgrade.add_user_state(FakeApi([], stored={**STORED, "b" * 32: {}}), MANIFEST)

    def test_a_user_still_listed_after_deletion_fails(self):
        with self.assertRaisesRegex(CheckError, "the user deletion"):
            upgrade.add_user_state(FakeApi([], names=[upgrade.CREATED, upgrade.DELETED]), MANIFEST)

    def test_every_missing_state_is_named(self):
        self.assertEqual(upgrade.state_failures({}, [], upgrade.user_state(MANIFEST)),
                         ["the played movie", "the resume position", "the favourite", "the created user"])


class BootTest(unittest.TestCase):
    def test_a_clean_boot_says_nothing(self):
        self.assertEqual(upgrade.boot_failures('{"level":"INFO","fields":{"message":"database migrations applied"}}'), [])

    def test_missing_migrations_and_an_error_are_both_reported(self):
        self.assertEqual(upgrade.boot_failures('{"level":"ERROR","fields":{"message":"refused"}}'),
                         ["migrations did not complete", "ERROR in boot log"])


class UnseedTest(unittest.TestCase):
    def test_the_chain_goes_and_each_linked_container_keeps_only_its_live_member(self):
        db = Path(tempfile.mkdtemp()) / "jellyfin.db"
        live = {"Path": "/media/a.mkv", "ItemId": "f" * 32}
        stale = {"Path": "/media/a.mkv", "ItemId": "9" * 32}
        with closing(sqlite3.connect(db)) as conn, conn:
            conn.execute('CREATE TABLE "BaseItems" ("Id" TEXT, "Data" TEXT)')
            conn.execute('CREATE TABLE "UserData" ("ItemId" TEXT)')
            conn.execute('INSERT INTO "BaseItems" VALUES (?, ?)',
                         ("AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA", json.dumps({"Name": "x", "LinkedChildren": [live, stale]})))
            conn.execute('INSERT INTO "BaseItems" VALUES (?, NULL)', ("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",))
            conn.execute('INSERT INTO "UserData" VALUES (?)', ("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",))
        upgrade.unseed(db, {"orphans": ["b" * 32],
                            "linked_children": {"playlist": {"id": "a" * 32, "entries": [live, stale]}}})
        with closing(sqlite3.connect(db)) as conn:
            self.assertEqual([json.loads(row[0]) for row in conn.execute('SELECT "Data" FROM "BaseItems"')],
                             [{"Name": "x", "LinkedChildren": [live]}])
            self.assertEqual(conn.execute('SELECT COUNT(*) FROM "UserData"').fetchone()[0], 0)


class ComparableTest(unittest.TestCase):
    SEEDS = {"ratings": {"r" * 32: "FSK 16"}, "conflict": {"item": "k" * 32}}

    def snapshot(self, version, child_sees_rated=True, resumable=True):
        return {"versions": ["m" * 32, version],
                "visibility": {"synthetic-child": ["a" * 32] + ["r" * 32] * child_sees_rated},
                "views": {"synthetic-adult": {"resume": ["k" * 32] * resumable + ["b" * 32]}}}

    def test_only_the_repairs_the_candidate_makes_by_design_are_left_out(self):
        self.assertEqual(upgrade.comparable(self.snapshot("v" * 32), self.SEEDS),
                         upgrade.comparable(self.snapshot("w" * 32, False, False), self.SEEDS))

    def test_a_lost_version_or_an_unseeded_change_still_differs(self):
        before = upgrade.comparable(self.snapshot("v" * 32), self.SEEDS)
        lost = self.snapshot("v" * 32)
        lost["versions"].pop()
        self.assertNotEqual(before, upgrade.comparable(lost, self.SEEDS))
        hidden = self.snapshot("v" * 32)
        hidden["visibility"]["synthetic-child"].remove("a" * 32)
        self.assertNotEqual(before, upgrade.comparable(hidden, self.SEEDS))


class RunnerTest(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp())

    def test_no_start_release_is_refused(self):
        with self.assertRaisesRegex(CheckError, "--from"):
            upgrade.run(self.root, [], "candidate", None, 1)

    def test_a_start_that_cannot_run_does_not_hide_another_starts_failures(self):
        results = {"a": CheckError("server did not become ready"), "b": ["b scan: views"]}

        def fake(root, start, image, fixture, port):
            if isinstance(results[start], Exception):
                raise results[start]
            return results[start]
        original, upgrade.upgrade = upgrade.upgrade, fake
        try:
            with self.assertRaisesRegex(CheckError, "a: server did not become ready; b scan: views"):
                upgrade.run(self.root, ["a", "b"], "candidate", None, 1)
        finally:
            upgrade.upgrade = original

    def test_an_unbuilt_fixture_is_refused_before_any_copy(self):
        (self.root / upgrade.FIXTURE).mkdir()
        with self.assertRaisesRegex(CheckError, "not built"):
            upgrade.run(self.root, ["ghcr.io/x/ferrofin:v1.3.2"], "candidate", None, 1)
        self.assertFalse((self.root / "work").exists())

    def test_a_previous_run_is_never_overwritten(self):
        fixture = self.root / upgrade.FIXTURE
        fixture.mkdir()
        (fixture / "synthetic-ready.json").write_text(json.dumps({"fixture": upgrade.FIXTURE}))
        (fixture / "synthetic.json").write_text(json.dumps(MANIFEST))
        (self.root / "work" / upgrade.work_name("ghcr.io/x/ferrofin:v1.3.2", upgrade.FIXTURE)).mkdir(parents=True)
        with self.assertRaisesRegex(CheckError, "previous upgrade run remains"):
            upgrade.run(self.root, ["ghcr.io/x/ferrofin:v1.3.2"], "candidate", None, 1)

    def test_each_start_gets_its_own_work_directory(self):
        self.assertEqual(upgrade.work_name("ghcr.io/x/ferrofin:v1.3.2", "jellyfin-10.11.8"),
                         "upgrade-ferrofin_v1.3.2-jellyfin-10.11.8")
        self.assertNotEqual(upgrade.work_name("ghcr.io/x/ferrofin:v1.0.0", "f"),
                            upgrade.work_name("ghcr.io/x/ferrofin:v1.3.2", "f"))


class FlowApi:
    base = "http://127.0.0.1:1"

    def __init__(self, events):
        self.events = events

    def login(self, name):
        return self

    def idle(self):
        pass

    def scan(self, timeout, interval):
        self.events.append("scan")


class FlowTest(unittest.TestCase):
    """upgrade() end to end with a fake server: no docker, no database."""
    START, CANDIDATE = "ghcr.io/x/ferrofin:v1.3.2", "candidate"

    def setUp(self):
        self.root = Path(tempfile.mkdtemp())
        fixture = self.root / upgrade.FIXTURE
        (fixture / "data").mkdir(parents=True)
        (fixture / "synthetic-ready.json").write_text(json.dumps({"fixture": upgrade.FIXTURE}))
        (fixture / "synthetic.json").write_text(json.dumps({**MANIFEST, "seeds": {}}))
        self.work = self.root / "work" / upgrade.work_name(self.START, upgrade.FIXTURE)
        self.events, self.snapshot_error, self.changed = [], None, []

    @contextmanager
    def server(self, image, config, media, name, port, ferrofin=False):
        self.events.append(("boot", image, config))
        (config.parent / (config.name + "-cache")).mkdir(exist_ok=True)  # as synthetic.server does
        yield FlowApi(self.events)
        (config.parent / (config.name + ".server.log")).write_text(image)

    def snapshot(self, admin, manifest, images=None):
        if self.snapshot_error and not any(event == ("boot", self.CANDIDATE, self.work) for event in self.events):
            raise CheckError(self.snapshot_error)
        return {}

    def run_upgrade(self):
        with patch("synthetic.server", self.server), patch("preservation.snapshot", self.snapshot), \
                patch("preservation.verify_database"), patch("preservation.container_log",
                                                             return_value='"database migrations applied"'), \
                patch.object(upgrade, "unseed"), patch.object(upgrade, "stores", return_value={}), \
                patch.object(upgrade, "comparable", side_effect=lambda data, seeds: data), \
                patch.object(upgrade, "add_user_state", side_effect=lambda admin, manifest: manifest), \
                patch.object(upgrade, "differences", side_effect=lambda *args: list(self.changed)) as differences:
            return upgrade.upgrade(self.root, self.START, self.CANDIDATE, upgrade.FIXTURE, 1), differences

    def test_the_candidate_boots_on_the_start_releases_volume_and_a_pass_cleans_up(self):
        failures, _ = self.run_upgrade()
        self.assertEqual(failures, [])
        self.assertEqual(self.events, [("boot", self.START, self.work), "scan",
                                       ("boot", self.CANDIDATE, self.work), "scan"])
        self.assertEqual(list((self.root / "work").iterdir()), [])

    def test_a_failure_names_each_stage_and_keeps_the_copy_and_both_logs(self):
        self.changed = ["database missing item"]
        failures, _ = self.run_upgrade()
        self.assertEqual(failures, [f"{self.START} upgrade: database missing item",
                                    f"{self.START} scan: database missing item"])
        self.assertTrue(self.work.is_dir())
        self.assertEqual((self.work.parent / (self.work.name + ".start.server.log")).read_text(), self.START)
        self.assertEqual((self.work.parent / (self.work.name + ".server.log")).read_text(), self.CANDIDATE)

    def test_a_known_api_gap_compares_stored_state_only(self):
        self.snapshot_error = "synthetic fixture GET /Items/<id>/Download returned 200"
        failures, differences = self.run_upgrade()
        self.assertEqual(failures, [])
        self.assertEqual(len(differences.call_args_list), 2)
        self.assertTrue(all(call.args[2] is None and call.args[3] is None for call in differences.call_args_list))

    def test_any_other_api_failure_fails_the_run(self):
        self.snapshot_error = "synthetic fixture GET /Users returned 500"
        with self.assertRaisesRegex(CheckError, "/Users returned 500"):
            self.run_upgrade()


if __name__ == "__main__":
    unittest.main()
