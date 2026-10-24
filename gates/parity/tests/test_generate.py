#!/usr/bin/env python3
"""Self-tests of gates/parity/generate.py: the sweep covers every operation of the vendored contract, the
checked-in request set is what the generator writes, and the generator's rules. Stdlib only.

    python3 -m unittest discover -s gates/parity/tests
"""

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import diff  # noqa: E402
import generate  # noqa: E402

SWEEP = generate.OUT


def sweep_requests():
    return [json.loads(line) for file in sorted(SWEEP.glob("*.jsonl")) for line in file.read_text().splitlines() if line.strip()]


class Sweep(unittest.TestCase):
    def test_every_operation_of_the_vendored_contract_has_a_request(self):
        contract = json.loads(generate.CONTRACT.read_text())
        operations = {f"{method} {path}" for _, method, path, _ in generate.operations(contract)}
        self.assertEqual(len(operations), 412)
        self.assertEqual(operations - generate.covered(sweep_requests()), set())

    def test_the_checked_in_request_files_are_what_the_generator_writes(self):
        for name, text in generate.generate().items():
            with self.subTest(name):
                self.assertEqual((SWEEP / name).read_text(), text, "stale: run gates/parity/generate.py")

    def test_the_sweep_is_a_valid_request_set_whose_templates_resolve(self):
        ids = {"user": "u", "user_name": "n", "password": "p", "viewer": "v", "movie": "m", "hdr_movie": "h", "series": "s",
               "season": "se", "episode": "e", "views": {"movies": "vm", "tvshows": "vt", "music": "vu"}}
        spec = diff.load_discover(diff.HERE / "discover.toml")
        requests = diff.load_requests(SWEEP, diff.placeholder_ids(ids, spec))
        self.assertEqual(len({r.id for r in requests}), len(requests))

    def test_the_known_oracle_traps_are_marked(self):
        traps = {r["id"]: r.get("oracle_trap") for r in sweep_requests()}
        for op in ("CreateBackup", "GetYears", "GetQueryFiltersLegacy"):
            with self.subTest(op):
                self.assertTrue(traps[op])

    def test_jellyfin_web_screens_are_in_the_sweep(self):
        ids = {r["id"] for r in sweep_requests()}
        for screen in ("screen-home-nextup", "screen-movies-page-0", "screen-detail-item", "screen-series-episodes",
                       "screen-search-items", "screen-playback-start"):
            self.assertIn(screen, ids)


CONTRACT = {
    "paths": {
        "/Things/{itemId}": {"get": {"operationId": "GetThing", "parameters": [
            {"in": "path", "name": "itemId", "required": True},
            {"in": "query", "name": "kind", "required": True, "schema": {"enum": ["A", "B"]}},
            {"in": "query", "name": "flag", "schema": {"type": "boolean"}},
            {"in": "query", "name": "sort", "schema": {"type": "array", "items": {"$ref": "#/components/schemas/Sort"}}},
            {"in": "query", "name": "free", "schema": {"type": "string"}}], "responses": {"200": {}}}},
        "/Lists": {"post": {"operationId": "CreateList", "requestBody": {"required": True}, "responses": {"200": {}}}},
        "/Lists/{listId}": {"get": {"operationId": "GetList", "parameters": [{"in": "path", "name": "listId"}], "responses": {"200": {}}},
                            "delete": {"operationId": "DeleteList", "parameters": [{"in": "path", "name": "listId"}], "responses": {"204": {}}}},
        "/Other/Things/{itemId}": {"get": {"operationId": "GetThing", "parameters": [{"in": "path", "name": "itemId"}], "responses": {"200": {}}}},
    },
    "components": {"schemas": {"Sort": {"enum": ["Name", "Date"]}}},
}
CONFIG = '''
[path]
itemId = "{movie}"
listId = "{setup.list.Id}"
[path_by_prefix."/Other/"]
itemId = "{series}"
[query]
kind = "A"
[[setups.list]]
id = "list"
method = "POST"
path = "/Lists"
body = {}
[op.GetList]
readback_of = "CreateList"
path = "/Lists/{write.Id}"
[op.CreateList]
body = { Name = "x" }
[combos]
ops = ["GetThing"]
skip = []
[combos.values]
free = ["v1"]
'''


class Rules(unittest.TestCase):
    def setUp(self):
        self._dir = tempfile.TemporaryDirectory()
        self.addCleanup(self._dir.cleanup)
        self.dir = Path(self._dir.name)

    def run_generator(self, config=CONFIG, contract=None):
        (self.dir / "c.json").write_text(json.dumps(contract or CONTRACT))
        (self.dir / "s.toml").write_text(config)
        files = generate.generate(self.dir / "c.json", self.dir / "s.toml")
        return {name: [json.loads(line) for line in text.splitlines()] for name, text in files.items()}

    def test_parameters_statuses_setups_and_read_backs(self):
        requests = {r["id"]: r for r in self.run_generator()["contract.jsonl"]}
        self.assertEqual(sorted(requests), ["CreateList", "DeleteList", "GetThing", "GetThing_OtherThingsItemid"])
        self.assertEqual((requests["GetThing"]["path"], requests["GetThing"]["query"]), ("/Things/{movie}", {"kind": "A"}))
        self.assertEqual(requests["GetThing_OtherThingsItemid"]["path"], "/Other/Things/{series}")
        self.assertEqual(requests["DeleteList"]["expect_status"], 204)
        self.assertEqual([s["id"] for s in requests["DeleteList"]["setup"]], ["list"])
        self.assertEqual(requests["CreateList"]["body"], {"Name": "x"})
        (readback,) = requests["CreateList"]["readbacks"]
        self.assertEqual((readback["id"], readback["path"]), ("GetList", "/Lists/{write.Id}"))
        self.assertEqual(generate.covered(requests.values()), {"GET /Things/{itemId}", "POST /Lists", "GET /Lists/{listId}",
                                                               "DELETE /Lists/{listId}", "GET /Other/Things/{itemId}"})

    def test_a_request_covers_an_operation_only_when_its_method_and_path_are_that_operations(self):
        source = "contract: GET /Things/{itemId}"
        self.assertEqual(generate.covered([{"method": "GET", "path": "/Things/{movie}", "source": source}]), {"GET /Things/{itemId}"})
        for method, path in (("POST", "/Things/{movie}"), ("GET", "/Other/x"), ("GET", "/Things/a/b")):
            with self.subTest(method=method, path=path):
                self.assertEqual(generate.covered([{"method": method, "path": path, "source": source}]), set())

    def test_combinations_send_each_documented_value_one_parameter_at_a_time(self):
        combos = self.run_generator()["combos.jsonl"]
        self.assertEqual([r["query"] for r in combos], [{"kind": "A", "flag": True}, {"kind": "A", "flag": False},
                                                        {"kind": "A", "sort": "Name"}, {"kind": "A", "sort": "Date"}, {"kind": "A", "free": "v1"}])
        self.assertEqual(len({r["id"] for r in combos}), len(combos))

    def test_what_the_configuration_cannot_fill_is_an_error(self):
        cases = {
            "no value for the path parameter `listId`": CONFIG.replace('listId = "{setup.list.Id}"\n', ""),
            "no value for the required query parameter `kind`": CONFIG.replace('kind = "A"\n', ""),
            "unknown key": CONFIG + '[op.GetThing]\nbogus = 1\n',
            "names operations the contract does not have": CONFIG + '[op.Nope]\nbody = {}\n',
            "names no \\[setups.other\\]": CONFIG.replace("{setup.list.Id}", "{setup.other.Id}"),
            "only a read is sent as a read-back": CONFIG + '[op.DeleteList]\nreadback_of = "CreateList"\n',
        }
        for message, config in cases.items():
            with self.subTest(message), self.assertRaisesRegex(generate.GenerateError, message):
                self.run_generator(config)


if __name__ == "__main__":
    unittest.main()
