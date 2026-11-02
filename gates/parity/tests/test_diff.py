#!/usr/bin/env python3
"""Self-tests of gates/parity/diff.py: request loading, normalisation, comparison, the ledger, the cap and
the report, over fixture response pairs and two throwaway local HTTP servers. Stdlib only.

    python3 -m unittest discover -s gates/parity/tests
"""

import _thread
import contextlib
import io
import json
import signal
import sys
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import diff  # noqa: E402

IDS = {"user": "u1", "user_name": "bench", "password": "pw", "movie": "m1", "views": {"movies": "v1"}}
VERIFIED_SOURCE = '''
pub const VERIFIED: &[Verified] = &[
    Verified {
        method: "get",
        path: "/Shows/NextUp",
    },
    Verified { method: "post", path: "/Items/{itemId}/PlaybackInfo" },
];
'''


def write(directory, name, text):
    path = Path(directory) / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    return str(path)


class Scratch(unittest.TestCase):
    def setUp(self):
        self._dir = tempfile.TemporaryDirectory()
        self.addCleanup(self._dir.cleanup)
        self.dir = Path(self._dir.name)

    def rules(self, toml):
        return diff.load_rules(write(self.dir, "normalise.toml", toml))

    def ledger(self, toml):
        return diff.load_ledger(write(self.dir, "ledger.toml", toml), write(self.dir, "mod.rs", VERIFIED_SOURCE))


def pair(oracle, ferrofin, rules=()):
    used = set()
    return diff.compare_bodies(oracle, ferrofin, list(rules), used), used


def kinds(differences):
    return [(d.at, d.kind) for d in differences]


class Requests(Scratch):
    def load(self, *lines):
        return diff.load_requests(write(self.dir, "r.jsonl", "\n".join(lines) + "\n"))

    def test_a_request_is_loaded_and_its_templates_filled_from_the_ids(self):
        (request,) = self.load('{"id": "a", "method": "GET", "path": "/Users/{user}/Items/{movie}", '
                               '"query": {"ParentId": "{views.movies}", "Recursive": true, "Fields": ["Path", "Overview"], "Limit": 12}}')
        self.assertEqual(diff.target(request, IDS), "/Users/u1/Items/m1?ParentId=v1&Recursive=true&Fields=Path,Overview&Limit=12")

    def test_a_path_is_percent_encoded_after_its_templates_are_filled(self):
        (request,) = self.load('{"id": "a", "method": "GET", "path": "/Items/{name}/Caf\u00e9 x"}')
        self.assertEqual(diff.target(request, {**IDS, "name": "Am\u00e9lie"}), "/Items/Am%C3%A9lie/Caf%C3%A9%20x")

    def test_an_integer_template_may_be_offset(self):
        (request,) = self.load('{"id": "a", "method": "GET", "path": "/Items", "query": {"startIndex": "{counts.movies-1}", "n": "{counts.movies+2}"}}')
        self.assertEqual(diff.target(request, {**IDS, "counts": {"movies": 3001}}), "/Items?startIndex=3000&n=3003")
        with self.assertRaisesRegex(diff.ParityError, "`movie` is not an integer: it cannot be offset"):
            diff.target(self.load('{"id": "b", "method": "GET", "path": "/{movie-1}"}')[0], IDS)

    def test_a_query_given_as_pairs_repeats_a_parameter(self):
        (request,) = self.load('{"id": "a", "method": "GET", "path": "/MediaSegments/{movie}", "query": [["t", "Intro"], ["t", "Outro"], ["u", "{user}"]]}')
        self.assertEqual(diff.target(request, IDS), "/MediaSegments/m1?t=Intro&t=Outro&u=u1")
        for bad in ('[["t"]]', '[1]', '"t=1"'):
            with self.subTest(bad), self.assertRaisesRegex(diff.ParityError, "`query` is an object or a list of"):
                self.load('{"id": "a", "method": "GET", "path": "/x", "query": %s}' % bad)

    def test_a_head_is_a_read(self):
        (request,) = self.load('{"id": "h", "method": "HEAD", "path": "/Audio/{movie}/stream"}')
        self.assertFalse(request.is_write)

    def test_a_request_directory_is_every_jsonl_file_in_it_in_name_order_and_ids_stay_unique(self):
        write(self.dir, "set/b.jsonl", '{"id": "b1", "method": "GET", "path": "/b"}\n')
        write(self.dir, "set/a.jsonl", '{"id": "a1", "method": "GET", "path": "/a"}\n{"id": "a2", "method": "GET", "path": "/a"}\n')
        write(self.dir, "set/notes.txt", "not a request file")
        requests = diff.load_requests(self.dir / "set")
        self.assertEqual([(r.id, r.file) for r in requests], [("a1", "set/a.jsonl"), ("a2", "set/a.jsonl"), ("b1", "set/b.jsonl")])
        write(self.dir, "set/c.jsonl", '{"id": "a1", "method": "GET", "path": "/c"}\n')
        with self.assertRaisesRegex(diff.ParityError, "c.jsonl:1: request id `a1` is used twice"):
            diff.load_requests(self.dir / "set")
        (self.dir / "empty").mkdir()
        with self.assertRaisesRegex(diff.ParityError, "no requests"):
            diff.load_requests(self.dir / "empty")

    def test_several_request_sets_are_sent_as_one_in_the_order_given_and_ids_stay_unique(self):
        a = write(self.dir, "a.jsonl", '{"id": "a1", "method": "GET", "path": "/a"}\n')
        write(self.dir, "set/b.jsonl", '{"id": "b1", "method": "GET", "path": "/b"}\n')
        self.assertEqual([r.id for r in diff.load_requests([str(self.dir / "set"), a])], ["b1", "a1"])
        with self.assertRaisesRegex(diff.ParityError, "request id `a1` is used twice"):
            diff.load_requests([a, a])

    def test_the_tracer_request_file_shipped_with_the_harness_is_valid(self):
        (request,) = diff.load_requests(diff.HERE / "requests" / "tracer.jsonl")
        self.assertEqual(request.id, "detail-item")
        self.assertEqual(diff.target(request, IDS), "/Users/u1/Items/m1")

    def test_every_template_can_be_resolved_up_front_against_the_ids(self):
        diff.load_requests(write(self.dir, "ok.jsonl", '{"id": "a", "method": "GET", "path": "/Users/{user}"}\n'), IDS)
        with self.assertRaisesRegex(diff.ParityError, "no `nothing`"):
            diff.load_requests(write(self.dir, "bad.jsonl", '{"id": "a", "method": "GET", "path": "/{nothing}"}\n'), IDS)

    def test_a_template_the_ids_file_cannot_fill_is_an_error(self):
        (request,) = self.load('{"id": "a", "method": "GET", "path": "/Items/{nothing}"}')
        with self.assertRaisesRegex(diff.ParityError, "no `nothing`"):
            diff.target(request, IDS)
        (request,) = self.load('{"id": "a", "method": "GET", "path": "/Items/{views}"}')
        with self.assertRaisesRegex(diff.ParityError, "not a string or an integer"):
            diff.target(request, IDS)

    def test_invalid_request_files_are_refused_with_the_line(self):
        cases = {
            "not JSON": ["{nope"],
            "a JSON object": ["[1]"],
            "unknown field": ['{"id": "a", "method": "GET", "path": "/x", "bogus": 1}'],
            "`id` is a non-empty string": ['{"method": "GET", "path": "/x"}'],
            "used twice": ['{"id": "a", "method": "GET", "path": "/x"}', '{"id": "a", "method": "GET", "path": "/y"}'],
            "is not a method the harness sends": ['{"id": "a", "method": "PUT", "path": "/x"}'],
            "starts with a slash": ['{"id": "a", "method": "GET", "path": "x"}'],
            "`query` is an object or a list of": ['{"id": "a", "method": "GET", "path": "/x", "query": [1]}'],
            "carries no body": ['{"id": "a", "method": "GET", "path": "/x", "body": {}}'],
            "`expect_status` is an HTTP status code": ['{"id": "a", "method": "GET", "path": "/x", "expect_status": "200"}',],
        }
        for message, lines in cases.items():
            with self.subTest(message), self.assertRaisesRegex(diff.ParityError, message):
                self.load(*lines)
        with self.assertRaisesRegex(diff.ParityError, "no requests"):
            self.load("")


class Writes(Scratch):
    def load(self, *lines, ids=None):
        return diff.load_requests(write(self.dir, "w.jsonl", "\n".join(lines) + "\n"), ids)

    def test_a_write_carries_a_body_and_read_backs_whose_ids_are_qualified_by_the_write(self):
        (request,) = self.load('{"id": "fav", "method": "POST", "path": "/UserFavoriteItems/{movie}", "query": {"userId": "{user}"}, '
                               '"body": {"Ids": ["{movie}"], "Name": "x {user}", "N": 3}, '
                               '"readbacks": [{"id": "data", "path": "/UserItems/{movie}/UserData", "expect_status": 200}, '
                               '{"id": "list", "path": "/Items", "query": {"IsFavorite": true}}]}', ids=IDS)
        self.assertTrue(request.is_write)
        self.assertEqual([r.id for r in request.readbacks], ["fav/data", "fav/list"])
        self.assertEqual({r.of for r in request.readbacks}, {"fav"})
        self.assertEqual(diff.target(request, IDS), "/UserFavoriteItems/m1?userId=u1")
        self.assertEqual(json.loads(diff.body_of(request, IDS)), {"Ids": ["m1"], "Name": "x u1", "N": 3})
        self.assertEqual(diff.target(request.readbacks[1], IDS), "/Items?IsFavorite=true")

    def test_a_write_may_set_up_its_fresh_copy_with_writes_of_its_own(self):
        (request,) = self.load('{"id": "unfav", "method": "DELETE", "path": "/UserFavoriteItems/{movie}", '
                               '"setup": [{"id": "fav", "method": "POST", "path": "/UserFavoriteItems/{movie}", "body": {"A": "{user}"}}]}', ids=IDS)
        (setup,) = request.setup
        self.assertEqual((setup.id, setup.of, setup.method, diff.target(setup, IDS)), ("unfav/fav", "unfav", "POST", "/UserFavoriteItems/m1"))
        self.assertEqual(diff.every_id([request]), ["unfav", "unfav/fav"])
        cases = {
            "a setup step has no body unless it is a write": '{"id": "a", "method": "POST", "path": "/x", "setup": [{"id": "s", "method": "GET", "path": "/y", "body": {}}]}',
            "only a write has setup steps": '{"id": "a", "method": "GET", "path": "/x", "setup": [{"id": "s", "method": "POST", "path": "/y"}]}',
            "setup: unknown field": '{"id": "a", "method": "POST", "path": "/x", "setup": [{"id": "s", "method": "POST", "path": "/y", "readbacks": []}]}',
            "id `s` is used twice": '{"id": "a", "method": "POST", "path": "/x", "setup": [{"id": "s", "method": "POST", "path": "/y"}], "readbacks": [{"id": "s", "path": "/z"}]}',
            "`setup` is a list": '{"id": "a", "method": "POST", "path": "/x", "setup": {}}',
        }
        for message, line in cases.items():
            with self.subTest(message), self.assertRaisesRegex(diff.ParityError, message):
                self.load(line)

    def test_a_delete_and_a_write_without_a_body_or_read_backs_are_writes_too(self):
        (request,) = self.load('{"id": "d", "method": "DELETE", "path": "/UserPlayedItems/{movie}"}')
        self.assertEqual((request.is_write, request.readbacks, request.setup, diff.body_of(request, IDS)), (True, (), (), None))

    def test_a_read_back_reads_what_each_server_answered_to_the_write(self):
        (request,) = self.load('{"id": "pl", "method": "POST", "path": "/Playlists", "readbacks": '
                               '[{"id": "items", "path": "/Playlists/{write.Id}/Items", "query": {"userId": "{user}", "x": "{write.Nested.N}"}}]}', ids=IDS)
        readback = request.readbacks[0]
        self.assertEqual(diff.target(readback, IDS, {"write": {"Id": "p-oracle", "Nested": {"N": 4}}}), "/Playlists/p-oracle/Items?userId=u1&x=4")
        self.assertEqual(diff.target(readback, IDS, {"write": {"Id": "p-ferrofin", "Nested": {"N": 4}}}), "/Playlists/p-ferrofin/Items?userId=u1&x=4")
        for answer in (None, {}, {"Id": None}, {"Id": {"a": 1}}, [1]):
            with self.subTest(answer), self.assertRaisesRegex(diff.CaptureMissing, "write.Id"):
                diff.target(readback, IDS, {"write": answer})

    def test_write_captures_are_only_for_read_backs(self):
        for line in ('{"id": "a", "method": "GET", "path": "/X/{write.Id}"}',
                     '{"id": "a", "method": "GET", "path": "/X/{setup.s.Id}"}'):
            with self.subTest(line), self.assertRaisesRegex(diff.ParityError, "only in a write's setup steps and read-backs"):
                self.load(line, ids=IDS)

    def test_setup_answers_are_captured_for_later_steps_the_write_and_its_read_backs(self):
        (request,) = self.load('{"id": "w", "method": "POST", "path": "/Playlists/{setup.pl.Id}/Items", "query": {"ids": "{movie}"}, '
                               '"setup": [{"id": "pl", "method": "POST", "path": "/Playlists", "body": {"Name": "x"}}, '
                               '{"id": "me", "method": "GET", "path": "/Sessions", "query": {"deviceId": "parity"}}, '
                               '{"id": "share", "method": "POST", "path": "/Playlists/{setup.pl.Id}/Users/{setup.me.0.UserId}"}], '
                               '"readbacks": [{"id": "r", "path": "/Playlists/{setup.pl.Id}/Items", "query": {"n": "{write.N}"}}]}', ids=IDS)
        pl, me, share = request.setup
        self.assertEqual((me.method, me.is_write), ("GET", False))
        captures = {"setup.pl": {"Id": "p1"}, "setup.me": [{"UserId": "u9"}], "write": {"N": 2}}
        self.assertEqual(diff.target(share, IDS, captures), "/Playlists/p1/Users/u9")
        self.assertEqual(diff.target(request, IDS, captures), "/Playlists/p1/Items?ids=m1")
        self.assertEqual(diff.target(request.readbacks[0], IDS, captures), "/Playlists/p1/Items?n=2")
        with self.assertRaisesRegex(diff.CaptureMissing, "setup.pl.Id"):
            diff.target(share, IDS, {"setup.me": [{"UserId": "u9"}]})

    def test_a_capture_may_only_name_a_step_sent_before_it(self):
        cases = {
            # a setup step reading a later one, the write reading its own answer, an unknown step
            "names no step sent before it": '{"id": "w", "method": "POST", "path": "/x", "setup": [{"id": "a", "method": "POST", "path": "/{setup.b.Id}"}, {"id": "b", "method": "POST", "path": "/y"}]}',
            "names no step sent before": '{"id": "w", "method": "POST", "path": "/x/{write.Id}"}',
            "no step sent before": '{"id": "w", "method": "POST", "path": "/x", "readbacks": [{"id": "r", "path": "/{setup.nope.Id}"}]}',
        }
        for message, line in cases.items():
            with self.subTest(line), self.assertRaisesRegex(diff.ParityError, message):
                self.load(line, ids=IDS)

    def test_invalid_writes_and_read_backs_are_refused(self):
        cases = {
            "only a write has read-backs": '{"id": "a", "method": "GET", "path": "/x", "readbacks": [{"id": "r", "path": "/y"}]}',
            "`readbacks` is a list": '{"id": "a", "method": "POST", "path": "/x", "readbacks": {}}',
            "read-back is a JSON object": '{"id": "a", "method": "POST", "path": "/x", "readbacks": [1]}',
            "read-back: unknown field": '{"id": "a", "method": "POST", "path": "/x", "readbacks": [{"id": "r", "path": "/y", "method": "POST"}]}',
            "read-back: `id` is a non-empty string": '{"id": "a", "method": "POST", "path": "/x", "readbacks": [{"path": "/y"}]}',
            "id `r` is used twice": '{"id": "a", "method": "POST", "path": "/x", "readbacks": [{"id": "r", "path": "/y"}, {"id": "r", "path": "/z"}]}',
            "read-back: `path` starts with a slash": '{"id": "a", "method": "POST", "path": "/x", "readbacks": [{"id": "r", "path": "y"}]}',
            "`body` is a JSON object, an array or null": '{"id": "a", "method": "POST", "path": "/x", "body": "text"}',
            "has no `/` in it": '{"id": "a/b", "method": "GET", "path": "/x"}',
        }
        for message, line in cases.items():
            with self.subTest(message), self.assertRaisesRegex(diff.ParityError, message):
                self.load(line)

    def test_the_starter_write_set_shipped_with_the_harness_is_valid_and_covers_the_listed_kinds(self):
        requests = diff.load_requests(diff.HERE / "requests" / "writes.jsonl",
                                      {**IDS, "hdr_movie": "m2", "episode": "e1", "series": "s1"})
        self.assertTrue(all(r.is_write and r.readbacks for r in requests))
        # a DELETE clears state its setup step made on the fresh copy: it is never a no-op
        self.assertTrue(all(r.setup for r in requests if r.method == "DELETE"))
        paths = " ".join(r.path for r in requests)
        for kind in ("/UserPlayedItems/", "/UserFavoriteItems/", "/UserData", "/Playlists", "/Collections"):
            self.assertIn(kind, paths)


class Discovery(Scratch):
    SPEC = ('[found.genre]\npath = "/Genres"\nquery = {Limit = 1, userId = "{user}"}\npick = "Items.0.Name"\n'
            '[found.person]\npath = "/Persons"\npick = "Items.0.Name"\n')

    def test_names_are_read_from_the_oracle_and_filled_into_templates(self):
        spec = diff.load_discover(write(self.dir, "d.toml", self.SPEC))
        servers = FakeServers({("o", "/Genres?Limit=1&userId=u1"): {"Items": [{"Name": "Drama"}]},
                               ("o", "/Persons"): {"Items": [{"Name": "Ann Lee"}]}})
        found = diff.discover("o", "t", spec, IDS, 30, send=servers)
        self.assertEqual(found, {"genre": "Drama", "person": "Ann Lee"})
        (request,) = diff.load_requests(write(self.dir, "r.jsonl", '{"id": "g", "method": "GET", "path": "/Genres/{found.genre}"}\n'))
        self.assertEqual(diff.target(request, {**IDS, "found": found}), "/Genres/Drama")

    def test_a_name_the_oracle_does_not_give_stops_the_run(self):
        spec = diff.load_discover(write(self.dir, "d.toml", self.SPEC))
        for answer in ({"Items": []}, diff.Response(500, b"", 0.1), {"Items": [{"Name": [5]}]}):
            servers = FakeServers({("o", "/Genres?Limit=1&userId=u1"): answer, ("o", "/Persons"): {"Items": [{"Name": "x"}]}})
            with self.subTest(answer), self.assertRaisesRegex(diff.ParityError, "cannot discover `genre`"):
                diff.discover("o", "t", spec, IDS, 30, send=servers)

    def test_invalid_discovery_files_are_refused_and_templates_are_checked_against_them(self):
        for toml, message in (('[found.g]\npick = "x"\n', "`path` starts with a slash"), ('[found.g]\npath = "/x"\n', "`pick` is"),
                              ('[found.g]\npath = "/x"\npick = "a"\nextra = 1\n', "unknown key"), ('[other]\n', "unknown key")):
            with self.subTest(toml), self.assertRaisesRegex(diff.ParityError, message):
                diff.load_discover(write(self.dir, "bad.toml", toml))
        self.assertEqual(diff.placeholder_ids(IDS, diff.load_discover(write(self.dir, "d.toml", self.SPEC)))["found"], {"genre": "x", "person": "x"})
        with self.assertRaisesRegex(diff.ParityError, "no `found.nobody`"):
            diff.load_requests(write(self.dir, "r.jsonl", '{"id": "a", "method": "GET", "path": "/{found.nobody}"}\n'),
                               diff.placeholder_ids(IDS, diff.load_discover(write(self.dir, "d.toml", self.SPEC))))

    def test_only_the_names_a_request_set_uses_are_discovered(self):
        requests = diff.load_requests(write(self.dir, "r.jsonl", '{"id": "a", "method": "GET", "path": "/Genres/{found.genre}"}\n'
                                            '{"id": "w", "method": "POST", "path": "/x", "setup": [{"id": "s", "method": "GET", "path": "/{found.studio}"}], '
                                            '"readbacks": [{"id": "r", "path": "/y", "query": {"q": "{found.person}"}}]}\n'))
        self.assertEqual(diff.used_names(requests), {"genre", "studio", "person"})


class Concurrency(Scratch):
    def test_reads_run_n_at_a_time_per_server_and_are_reported_in_file_order(self):
        lock, in_flight, peak = threading.Lock(), {"o": 0, "f": 0}, {"o": 0, "f": 0}

        def send(base, method, path, headers, timeout, now=None, body=None):
            with lock:
                in_flight[base] += 1
                peak[base] = max(peak[base], in_flight[base])
            time.sleep(0.05)
            with lock:
                in_flight[base] -= 1
            return diff.Response(200, json.dumps({"P": path}).encode(), 0.05)

        requests = [diff.Request(f"r{i}", "GET", f"/r{i}", {}, None, "", "", 200) for i in range(12)]
        outcome = diff.run_requests(requests, IDS, "o", "f", {"oracle": "t", "ferrofin": "t"}, [], [], 30, None, send=send, concurrency=3)
        self.assertEqual([r["id"] for r in outcome.results], [f"r{i}" for i in range(12)])
        self.assertEqual(peak, {"o": 3, "f": 3})
        self.assertEqual((outcome.timing["reads"]["requests"], outcome.timing["reads"]["concurrency"]), (12, 3))

    def test_an_interrupted_concurrent_run_cancels_the_reads_still_queued(self):
        sent = []

        def send(base, method, path, headers, timeout, now=None, body=None):
            sent.append(path)
            if len(sent) == 3:
                _thread.interrupt_main()   # as a signal handler's exit is raised in the main thread while reads are queued
            time.sleep(0.02)
            return diff.Response(200, b"{}", 0.0)

        requests = [diff.Request(f"r{i}", "GET", f"/r{i}", {}, None, "", "", 200) for i in range(200)]
        # interrupt_main() does nothing while SIGINT is ignored, as it is for a job a script starts in the
        # background (the pre-push CI mirror runs these tests so): Python's own handler, for this test.
        self.addCleanup(signal.signal, signal.SIGINT, signal.signal(signal.SIGINT, signal.default_int_handler))
        with self.assertRaises(KeyboardInterrupt):
            diff.run_requests(requests, IDS, "o", "f", {"oracle": "t", "ferrofin": "t"}, [], [], 30, None, send=send, concurrency=2)
        self.assertLess(len(sent), 20)

    def test_the_cap_stops_concurrent_reads_and_the_run_fails(self):
        clock = {"now": 100.0}

        def send(base, method, path, headers, timeout, now=None, body=None):
            clock["now"] += 10
            return diff.Response(200, b"{}", 0.0)

        requests = [diff.Request(f"r{i}", "GET", f"/r{i}", {}, None, "", "", 200) for i in range(20)]
        outcome = diff.run_requests(requests, IDS, "o", "f", {"oracle": "t", "ferrofin": "t"}, [], [], 30, 150.0,
                                    wall=lambda: clock["now"], send=send, concurrency=4)
        self.assertTrue(outcome.cap_exceeded)
        self.assertLess(len(outcome.results), 20)

    def test_concurrency_is_held_to_its_range(self):
        for bad in (0, 9):
            with self.subTest(bad), self.assertRaisesRegex(diff.ParityError, "1 to 8"):
                diff.run_requests([], IDS, "o", "f", {}, [], [], 30, None, concurrency=bad)


class Comparison(Scratch):
    def test_identical_bodies_have_no_difference(self):
        body = {"Id": "a", "Items": [{"Name": "x", "N": 1}], "Flag": True, "Nothing": None}
        self.assertEqual(pair(body, json.loads(json.dumps(body)))[0], [])

    def test_a_value_a_type_and_a_length_are_differences_at_their_path(self):
        differences, _ = pair({"Name": "a", "Count": 1, "Items": [1, 2], "Flag": True},
                              {"Name": "b", "Count": "1", "Items": [1], "Flag": 1})
        self.assertEqual(sorted(kinds(differences)), [("Count", "type"), ("Flag", "type"), ("Items", "length"), ("Name", "value")])

    def test_a_value_that_becomes_null_is_reported_inside_an_otherwise_matching_object(self):
        oracle = {"Item": {"Name": "x", "Overview": "text", "Year": 2020}, "Total": 1}
        ferrofin = {"Item": {"Name": "x", "Overview": None, "Year": 2020}, "Total": 1}
        self.assertEqual(kinds(pair(oracle, ferrofin)[0]), [("Item.Overview", "null")])
        self.assertEqual(kinds(pair(ferrofin, oracle)[0]), [("Item.Overview", "null")])

    def test_a_missing_and_an_extra_field_are_both_differences(self):
        differences, _ = pair({"A": 1, "B": 2}, {"A": 1, "C": 3})
        self.assertEqual(sorted(kinds(differences)), [("B", "missing"), ("C", "extra")])

    def test_array_elements_are_compared_in_order_and_reported_by_index_and_by_generalised_path(self):
        differences, _ = pair({"Items": [{"Id": 1}, {"Id": 2}]}, {"Items": [{"Id": 1}, {"Id": 3}]})
        (found,) = differences
        self.assertEqual((found.at, found.path, found.kind), ("Items[1].Id", "Items[].Id", "value"))

    def test_two_different_items_in_one_place_are_one_difference_at_the_id(self):
        oracle = {"Items": [{"Id": "a", "Name": "x", "Year": 1}, {"Id": "b", "Name": "y", "Year": 2}]}
        ferrofin = {"Items": [{"Id": "a", "Name": "x", "Year": 9}, {"Id": "c", "Name": "z", "Extra": 1}]}
        differences, _ = pair(oracle, ferrofin)
        self.assertEqual(sorted((d.at, d.path, d.kind, d.oracle, d.ferrofin) for d in differences),
                         [("Items[0].Year", "Items[].Year", "value", "1", "9"),
                          ("Items[1].Id", "Items[].Id", "value", '"b"', '"c"')])

    def test_the_same_items_in_another_order_are_one_order_difference_and_each_item_is_compared_with_its_own(self):
        oracle = {"Items": [{"Id": "a", "N": 1}, {"Id": "b", "N": 2}, {"Id": "c", "N": 3}]}
        ferrofin = {"Items": [{"Id": "C", "N": 3}, {"Id": "a", "N": 1}, {"Id": "b", "N": 9}]}
        differences, _ = pair(oracle, ferrofin)
        self.assertEqual(sorted((d.at, d.path, d.kind, d.oracle, d.ferrofin) for d in differences),
                         [("Items", "Items", "order", '"[0] a of 3"', '"[0] c of 3"'),
                          ("Items[1].N", "Items[].N", "value", "2", "9"),
                          ("Items[2].Id", "Items[].Id", "value", '"c"', '"C"')])

    def test_a_root_array_reordered_is_an_order_difference_at_the_root(self):
        differences, _ = pair([{"Id": "a"}, {"Id": "b"}], [{"Id": "b"}, {"Id": "a"}])
        self.assertEqual(kinds(differences), [("$", "order")])

    def test_items_are_matched_by_the_first_key_every_item_has(self):
        oracle = [{"ItemId": "x", "Key": "1"}, {"ItemId": "y", "Key": "2"}]
        ferrofin = [{"ItemId": "y", "Key": "2"}, {"ItemId": "x", "Key": "1"}]
        self.assertEqual(kinds(pair(oracle, ferrofin)[0]), [("$", "order")])

    def test_a_late_reorder_names_where_the_orders_part(self):
        ids = [{"Id": str(n)} for n in range(10)]
        (found,) = pair({"Items": ids}, {"Items": ids[:8] + ids[:7:-1]})[0]
        self.assertEqual((found.kind, found.oracle, found.ferrofin), ("order", '"[8] 8 of 10"', '"[8] 9 of 10"'))

    def test_an_opaque_id_falls_through_to_the_next_key(self):
        rules = self.rules('[[rule]]\npaths = ["Items[].Id"]\nkind = "opaque"\nreason = "per server"\n')
        differences, _ = pair({"Items": [{"Id": "x", "Key": "a"}, {"Id": "y", "Key": "b"}]},
                              {"Items": [{"Id": "z", "Key": "b"}, {"Id": "w", "Key": "a"}]}, rules)
        self.assertEqual(kinds(differences), [("Items", "order")])

    def test_ids_that_collide_once_dashes_and_case_are_set_aside_compare_by_position(self):
        differences, _ = pair({"Items": [{"Id": "a-b", "N": 1}, {"Id": "AB", "N": 2}]},
                              {"Items": [{"Id": "AB", "N": 2}, {"Id": "a-b", "N": 1}]})
        self.assertEqual(sorted(kinds(differences)),
                         [("Items[0].Id", "value"), ("Items[0].N", "value"), ("Items[1].Id", "value"), ("Items[1].N", "value")])

    def test_arrays_of_arrays_and_mixed_arrays_compare_by_position(self):
        self.assertEqual(kinds(pair({"L": [[1], [2]]}, {"L": [[2], [1]]})[0]), [("L[0][0]", "value"), ("L[1][0]", "value")])
        self.assertEqual(kinds(pair({"L": [{"Id": "a"}, "b"]}, {"L": ["b", {"Id": "a"}]})[0]), [("L[0]", "type"), ("L[1]", "type")])

    def test_an_unordered_rule_pairs_its_array_itself(self):
        rules = self.rules('[[rule]]\npaths = ["Items"]\nkind = "unordered"\nby = "Name"\nreason = "no order"\n')
        differences, used = pair({"Items": [{"Id": "a", "Name": "x"}, {"Id": "b", "Name": "y"}]},
                                 {"Items": [{"Id": "b", "Name": "y"}, {"Id": "a", "Name": "x"}]}, rules)
        self.assertEqual((differences, len(used)), ([], 1))

    def test_scalars_in_another_order_are_an_order_difference(self):
        self.assertEqual(kinds(pair({"Tags": ["a", "b", "a"]}, {"Tags": ["a", "a", "b"]})[0]), [("Tags", "order")])

    def test_a_page_holding_other_items_is_not_a_reorder(self):
        differences, _ = pair({"Items": [{"Id": "a"}, {"Id": "b"}]}, {"Items": [{"Id": "b"}, {"Id": "c"}]})
        self.assertEqual(kinds(differences), [("Items[0].Id", "value"), ("Items[1].Id", "value")])

    def test_a_repeated_or_missing_key_compares_by_position(self):
        repeated = pair({"Items": [{"Id": "a", "N": 1}, {"Id": "a", "N": 2}]}, {"Items": [{"Id": "a", "N": 2}, {"Id": "a", "N": 1}]})[0]
        self.assertEqual(kinds(repeated), [("Items[0].N", "value"), ("Items[1].N", "value")])
        keyless = pair({"Items": [{"N": 1}, {"N": 2}]}, {"Items": [{"N": 2}, {"N": 1}]})[0]
        self.assertEqual(kinds(keyless), [("Items[0].N", "value"), ("Items[1].N", "value")])

    def test_an_opaque_only_key_or_a_rule_on_the_element_keeps_position(self):
        oracle = {"Items": [{"Id": "a", "N": 1}, {"Id": "b", "N": 2}]}
        ferrofin = {"Items": [{"Id": "b", "N": 2}, {"Id": "a", "N": 1}]}
        opaque_id = self.rules('[[rule]]\npaths = ["Items[].Id"]\nkind = "opaque"\nreason = "per server"\n')
        self.assertEqual(kinds(pair(oracle, ferrofin, opaque_id)[0]), [("Items[0].N", "value"), ("Items[1].N", "value")])
        element = self.rules('[[rule]]\npaths = ["Items[]"]\nkind = "opaque"\nreason = "per server"\n')
        self.assertEqual(pair(oracle, ferrofin, element)[0], [])

    def test_an_id_that_differs_only_in_its_format_is_the_same_item_and_its_fields_are_compared(self):
        differences, _ = pair({"Items": [{"Id": "0123abcd", "N": 1}]}, {"Items": [{"Id": "0123-ABCD", "N": 2}]})
        self.assertEqual(sorted(kinds(differences)), [("Items[0].Id", "value"), ("Items[0].N", "value")])

    def test_a_rule_on_the_element_turns_the_collapse_off(self):
        rules = self.rules('[[rule]]\npaths = ["Items[]"]\nkind = "opaque"\nreason = "per server"\n')
        differences, used = pair({"Items": [{"Id": "a", "N": 1}]}, {"Items": [{"Id": "b", "M": 1}]}, rules)
        self.assertEqual((differences, len(used)), ([], 1))

    def test_elements_are_compared_field_by_field_when_the_id_is_opaque_or_absent(self):
        rules = self.rules('[[rule]]\npaths = ["Items[].Id"]\nkind = "opaque"\nreason = "per server"\n')
        differences, _ = pair({"Items": [{"Id": "a", "N": 1}]}, {"Items": [{"Id": "b", "N": 2}]}, rules)
        self.assertEqual(kinds(differences), [("Items[0].N", "value")])
        self.assertEqual(kinds(pair({"Items": [{"N": 1}]}, {"Items": [{"N": 2}]})[0]), [("Items[0].N", "value")])
        self.assertEqual(sorted(kinds(pair({"Items": [{"Id": "a"}]}, {"Items": [{"Key": "a"}]})[0])),
                         [("Items[0].Id", "missing"), ("Items[0].Key", "extra")])

    def test_numbers_that_are_equal_are_equal_whatever_their_spelling(self):
        self.assertEqual(pair({"N": 1}, {"N": 1.0})[0], [])

    def test_a_root_that_is_not_an_object_is_compared_at_the_root(self):
        self.assertEqual(kinds(pair([1], [2])[0]), [("[0]", "value")])
        self.assertEqual(kinds(pair("a", None)[0]), [("$", "null")])

    def test_an_opaque_rule_accepts_a_different_value_but_not_a_different_type_or_a_null(self):
        rules = self.rules('[[rule]]\npaths = ["Items[].ServerId"]\nkind = "opaque"\nreason = "per server"\n')
        same_type, used = pair({"Items": [{"ServerId": "a"}]}, {"Items": [{"ServerId": "b"}]}, rules)
        self.assertEqual((same_type, len(used)), ([], 1))
        self.assertEqual(kinds(pair({"Items": [{"ServerId": "a"}]}, {"Items": [{"ServerId": None}]}, rules)[0]), [("Items[0].ServerId", "null")])
        self.assertEqual(kinds(pair({"Items": [{"ServerId": "a"}]}, {"Items": [{"ServerId": 5}]}, rules)[0]), [("Items[0].ServerId", "type")])

    def test_an_unordered_rule_matches_elements_by_key_and_still_compares_them(self):
        rules = self.rules('[[rule]]\npaths = ["Tags"]\nkind = "unordered"\nby = "Id"\nreason = "no order in the contract"\n')
        oracle = {"Tags": [{"Id": "a", "N": 1}, {"Id": "b", "N": 2}]}
        self.assertEqual(pair(oracle, {"Tags": [{"Id": "b", "N": 2}, {"Id": "a", "N": 1}]}, rules)[0], [])
        self.assertEqual(kinds(pair(oracle, {"Tags": [{"Id": "b", "N": 9}, {"Id": "a", "N": 1}]}, rules)[0]), [("Tags[1].N", "value")])
        self.assertEqual(kinds(pair(oracle, {"Tags": [{"Id": "a", "N": 1}]}, rules)[0]), [("Tags", "length"), ])

    def test_an_opaque_keys_rule_ignores_the_keys_but_not_the_values_or_the_count(self):
        rules = self.rules('[[rule]]\npaths = ["Hashes.Primary"]\nkind = "opaque-keys"\nreason = "keyed by the image tag"\n')
        oracle = {"Hashes": {"Primary": {"tagA": "h1", "tagB": "h2"}}}
        differences, used = pair(oracle, {"Hashes": {"Primary": {"z9": "h2", "a1": "h1"}}}, rules)
        self.assertEqual((differences, len(used)), ([], 1))
        self.assertEqual(kinds(pair(oracle, {"Hashes": {"Primary": {"z9": "h2", "a1": "OTHER"}}}, rules)[0]), [("Hashes.Primary.tagA", "value")])
        self.assertEqual(kinds(pair(oracle, {"Hashes": {"Primary": {"z9": "h2"}}}, rules)[0])[0], ("Hashes.Primary", "length"))
        self.assertEqual(kinds(pair(oracle, {"Hashes": {"Primary": None}}, rules)[0]), [("Hashes.Primary", "null")])

    def test_an_unordered_rule_over_elements_without_the_key_is_a_type_difference(self):
        rules = self.rules('[[rule]]\npaths = ["Tags"]\nkind = "unordered"\nby = "Id"\nreason = "r"\n')
        self.assertEqual(kinds(pair({"Tags": ["a"]}, {"Tags": ["a"]}, rules)[0]), [("Tags", "type")])

    def test_a_rule_at_the_root_applies_to_the_whole_body(self):
        rules = self.rules('[[rule]]\npaths = ["$"]\nkind = "unordered"\nby = "ItemId"\nrequests = ["r1"]\nreason = "no order"\n')
        differences, used = pair([{"ItemId": "a"}, {"ItemId": "b"}], [{"ItemId": "b"}, {"ItemId": "a"}], rules)
        self.assertEqual((differences, len(used)), ([], 1))
        with self.assertRaisesRegex(diff.ParityError, "only for the requests the rule names"):
            self.rules('[[rule]]\npaths = ["$"]\nkind = "opaque"\nreason = "r"\n')

    def test_a_rule_that_matches_nothing_is_not_used(self):
        rules = self.rules('[[rule]]\npaths = ["Gone"]\nkind = "opaque"\nreason = "r"\n')
        self.assertEqual(pair({"A": 1}, {"A": 1}, rules)[1], set())


class RuleFile(Scratch):
    def test_the_rule_file_shipped_with_the_harness_is_valid(self):
        self.assertTrue(diff.load_rules(diff.HERE / "normalise.toml"))

    def test_invalid_rules_are_refused(self):
        cases = {
            "no wildcards": '[[rule]]\npaths = ["Items[*].X"]\nkind = "opaque"\nreason = "r"\n',
            "lists the exact field paths": '[[rule]]\npaths = []\nkind = "opaque"\nreason = "r"\n',
            "`kind` is one of": '[[rule]]\npaths = ["A"]\nkind = "ignore"\nreason = "r"\n',
            "says why": '[[rule]]\npaths = ["A"]\nkind = "opaque"\n',
            "names the field its elements are keyed by": '[[rule]]\npaths = ["A"]\nkind = "unordered"\nreason = "r"\n',
            "already a path of rule 1": ('[[rule]]\npaths = ["A"]\nkind = "opaque"\nreason = "r"\n'
                                         '[[rule]]\npaths = ["A"]\nkind = "opaque"\nreason = "r"\n'),
            "unknown key": '[[rule]]\npaths = ["A"]\nkind = "opaque"\nreason = "r"\nextra = 1\n',
            "says why": '[[rule]]\npaths = ["A"]\nkind = "opaque"\nreason = 5\n',
            "keyed by": '[[rule]]\npaths = ["A"]\nkind = "unordered"\nby = 3\nreason = "r"\n',
        }
        for message, toml in cases.items():
            with self.subTest(message), self.assertRaisesRegex(diff.ParityError, message):
                self.rules(toml)

    def test_a_rule_scoped_to_requests_applies_to_those_requests_only(self):
        rules = self.rules('[[rule]]\npaths = ["Id"]\nkind = "opaque"\nrequests = ["pl", "pl/*"]\nreason = "a new id per server"\n'
                           '[[rule]]\npaths = ["ServerId"]\nkind = "opaque"\nreason = "per server"\n')
        self.assertEqual([r.paths for r in diff.rules_for(rules, "pl/items")], [("Id",), ("ServerId",)])
        self.assertEqual([r.paths for r in diff.rules_for(rules, "detail-item")], [("ServerId",)])
        self.assertEqual(pair({"Id": "a"}, {"Id": "b"}, diff.rules_for(rules, "pl"))[0], [])
        self.assertEqual(kinds(pair({"Id": "a"}, {"Id": "b"}, diff.rules_for(rules, "other"))[0]), [("Id", "value")])

    def test_invalid_request_scopes_are_refused(self):
        cases = {
            "`requests` lists request ids or globs": '[[rule]]\npaths = ["A"]\nkind = "opaque"\nrequests = []\nreason = "r"\n',
            "is already a path of rule 1 for every request": ('[[rule]]\npaths = ["A"]\nkind = "opaque"\nreason = "r"\n'
                                                              '[[rule]]\npaths = ["A"]\nkind = "opaque"\nrequests = ["x"]\nreason = "r"\n'),
        }
        for message, toml in cases.items():
            with self.subTest(message), self.assertRaisesRegex(diff.ParityError, message):
                self.rules(toml)

    def test_two_scoped_rules_may_share_a_path_but_never_on_the_same_request(self):
        rules = self.rules('[[rule]]\npaths = ["A"]\nkind = "opaque"\nrequests = ["x*"]\nreason = "r"\n'
                           '[[rule]]\npaths = ["A"]\nkind = "opaque"\nrequests = ["y"]\nreason = "r"\n')
        diff.check_rule_scopes(rules, ["x1", "y", "z"])
        with self.assertRaisesRegex(diff.ParityError, "`A` of request `xy` is a path of rules 1 and 3"):
            diff.check_rule_scopes(rules + self.rules('[[rule]]\npaths = ["A"]\nkind = "opaque"\nrequests = ["xy"]\nreason = "r"\n'), ["xy"])


class Normalised(Scratch):
    def test_a_body_is_shown_as_the_comparison_saw_it_with_the_rules_it_applied(self):
        rules = self.rules('[[rule]]\npaths = ["ServerId"]\nkind = "opaque"\nreason = "r"\n'
                           '[[rule]]\npaths = ["Tags"]\nkind = "unordered"\nby = "Id"\nreason = "r"\n'
                           '[[rule]]\npaths = ["Hashes"]\nkind = "opaque-keys"\nreason = "r"\n'
                           '[[rule]]\npaths = ["Gone"]\nkind = "opaque"\nreason = "r"\n')
        body = {"ServerId": "abc", "Tags": [{"Id": "b"}, {"Id": "a"}], "Hashes": {"t2": "h2", "t1": "h1"}, "Name": "x"}
        shown, applied = diff.normalised(body, rules)
        self.assertEqual(shown, {"ServerId": "<opaque>", "Tags": [{"Id": "a"}, {"Id": "b"}], "Hashes": {"<key 1>": "h1", "<key 2>": "h2"}, "Name": "x"})
        self.assertEqual(applied, ["Hashes", "ServerId", "Tags"])
        self.assertEqual(diff.normalised(None, rules), (None, []))


class Filed(Scratch):
    def filed(self, toml):
        return diff.load_filed(write(self.dir, "filed.toml", toml))

    def test_a_filed_difference_names_its_issue_and_stays_new(self):
        (entry,) = self.filed('[[filed]]\nrequest = "GetItems.*"\npath = "TotalRecordCount"\nkind = "value"\nissue = 830\n')
        hit = diff.Difference("TotalRecordCount", "TotalRecordCount", "value", "1", "2")
        self.assertEqual(diff.filed_issue([entry], "GetItems.sortBy.name", hit), 830)
        self.assertIsNone(diff.filed_issue([entry], "Other", hit))
        self.assertIsNone(diff.filed_issue([entry], "GetItems.x", diff.Difference("TotalRecordCount", "TotalRecordCount", "null", "1", None)))

    def test_a_filed_image_tag_group_matches_every_tag_of_it(self):
        (entry,) = self.filed('[[filed]]\nrequest = "*"\npath = "Items[].ImageBlurHashes.Primary.<tag>"\nkind = "missing"\nissue = 7\n')
        hit = diff.Difference("Items[].ImageBlurHashes.Primary." + "c" * 32, "x", "missing", "1", None)
        self.assertEqual(diff.filed_issue([entry], "any", hit), 7)

    def test_check_refuses_a_bad_links_file_before_anything_boots(self):
        files = Live.files(self)
        bad = write(self.dir, "bad-filed.toml", 'version = 1\n[[filed]]\nrequest = "a"\npath = "X"\nkind = "value"\n')
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            code = diff.main(["check", "--requests", files["requests"], "--normalise", files["normalise"], "--ledger", files["ledger"], "--filed", bad])
        self.assertEqual(code, 2)
        self.assertIn("`issue` is a GitLab issue number", stderr.getvalue())

    def test_invalid_filed_entries_are_refused(self):
        for toml, message in (('[[filed]]\nrequest = "a"\npath = "X"\nkind = "value"\n', "`issue` is a GitLab issue number"),
                              ('[[filed]]\npath = "X"\nkind = "value"\nissue = 1\n', "`request` is required"),
                              ('[[filed]]\nrequest = "a"\npath = "X"\nkind = "bogus"\nissue = 1\n', "`kind` is required"),
                              ('[[filed]]\nrequest = "a"\npath = "X"\nkind = "value"\nissue = 1\nextra = 2\n', "unknown key")):
            with self.subTest(message), self.assertRaisesRegex(diff.ParityError, message):
                self.filed(toml)

    def test_the_shipped_links_file_is_valid(self):
        diff.load_filed(diff.HERE / "filed.toml")


class Ledger(Scratch):
    def test_the_ledger_shipped_with_the_harness_is_valid(self):
        diff.load_ledger(diff.HERE / "ledger.toml")

    def test_an_entry_links_to_a_verified_row_or_an_issue(self):
        entries = self.ledger('[[entry]]\nrequest = "a"\npath = "X"\nverified = "GET /Shows/NextUp"\nkind = "value"\nreason = "r"\n'
                              '[[entry]]\nrequest = "b"\npath = "Y"\nissue = 12\nkind = "null"\nreason = "r"\n')
        self.assertEqual([e.link() for e in entries], ["VERIFIED GET /Shows/NextUp", "issue gl-12"])

    def test_an_order_entry_accepts_a_reorder_at_the_array_and_at_the_root(self):
        entries = self.ledger('[[entry]]\nrequest = "GetYears"\npath = "Items"\nissue = 862\nkind = "order"\nreason = "r"\n'
                              '[[entry]]\nrequest = "GetTasks"\npath = "$"\nissue = 428\nkind = "order"\nreason = "r"\n')
        (items,), _ = pair({"Items": [{"Id": "a"}, {"Id": "b"}]}, {"Items": [{"Id": "b"}, {"Id": "a"}]})
        (whole,), _ = pair([{"Id": "a"}, {"Id": "b"}], [{"Id": "b"}, {"Id": "a"}])
        self.assertEqual(diff.matching_entry(entries, "GetYears", items), entries[0])
        self.assertEqual(diff.matching_entry(entries, "GetTasks", whole), entries[1])
        self.assertIsNone(diff.matching_entry(entries, "GetYears", whole))   # an order entry names its array

    def test_an_order_difference_the_ledger_does_not_accept_is_new(self):
        order = {"path": "Items", "kind": "order", "ledger": None, "filed": None}
        self.assertEqual(diff.outcome({"state": "differences", "differences": [order]}), "new")
        self.assertEqual(diff.outcome({"state": "differences", "differences": [{**order, "filed": 861}]}), "filed")

    def test_the_method_of_a_verified_link_is_not_case_sensitive(self):
        self.ledger('[[entry]]\nrequest = "a"\npath = "X"\nverified = "post /Items/{itemId}/PlaybackInfo"\nkind = "value"\nreason = "r"\n')

    def test_an_entry_without_a_link_or_with_two_fails(self):
        for body in ('', 'verified = "GET /Shows/NextUp"\nissue = 3\n'):
            with self.subTest(body), self.assertRaisesRegex(diff.ParityError, "exactly one link"):
                self.ledger(f'[[entry]]\nrequest = "a"\npath = "X"\nkind = "value"\nreason = "r"\n{body}')

    def test_a_verified_link_that_is_not_a_row_of_the_record_fails(self):
        with self.assertRaisesRegex(diff.ParityError, "not a row of the parity record"):
            self.ledger('[[entry]]\nrequest = "a"\npath = "X"\nverified = "GET /Nope"\nkind = "value"\nreason = "r"\n')

    def test_an_issue_must_be_a_positive_integer_and_a_kind_must_be_known(self):
        with self.assertRaisesRegex(diff.ParityError, "GitLab issue number"):
            self.ledger('[[entry]]\nrequest = "a"\npath = "X"\nissue = "12"\nkind = "value"\nreason = "r"\n')
        for kind in ('kind = "bogus"\n', 'kind = []\n', ''):
            with self.subTest(kind), self.assertRaisesRegex(diff.ParityError, "`kind` is required"):
                self.ledger(f'[[entry]]\nrequest = "a"\npath = "X"\nissue = 1\n{kind}reason = "r"\n')
        for verified in ('verified = 5\n', 'verified = ""\n'):
            with self.subTest(verified), self.assertRaisesRegex(diff.ParityError, "<METHOD> <path>"):
                self.ledger(f'[[entry]]\nrequest = "a"\npath = "X"\nkind = "value"\n{verified}reason = "r"\n')

    def test_an_entry_needs_a_request_a_path_and_a_reason(self):
        for missing in ("request", "path", "reason"):
            fields = {"request": '"a"', "path": '"X"', "reason": '"r"'}
            del fields[missing]
            text = '[[entry]]\nissue = 1\nkind = "value"\n' + "".join(f"{k} = {v}\n" for k, v in fields.items())
            with self.subTest(missing), self.assertRaisesRegex(diff.ParityError, f"`{missing}` is required"):
                self.ledger(text)

    def test_the_real_parity_record_is_readable_and_a_file_without_it_is_refused(self):
        self.assertIn("GET /Shows/NextUp", diff.verified_rows())
        with self.assertRaisesRegex(diff.ParityError, "no `pub const VERIFIED:`"):
            diff.verified_rows(write(self.dir, "empty.rs", "fn main() {}"))

    def test_matching_by_request_glob_path_and_kind(self):
        (entry,) = self.ledger('[[entry]]\nrequest = "detail-*"\npath = "A.B"\nkind = ["value", "null"]\nissue = 1\nreason = "r"\n')
        hit = diff.Difference("A.B", "A.B", "null", "x", "y")
        self.assertIs(diff.matching_entry([entry], "detail-item", hit), entry)
        self.assertIsNone(diff.matching_entry([entry], "other", hit))
        self.assertIsNone(diff.matching_entry([entry], "detail-item", diff.Difference("A.C", "A.C", "null", "x", "y")))
        self.assertIsNone(diff.matching_entry([entry], "detail-item", diff.Difference("A.B", "A.B", "type", "x", "y")))


class FakeServers:
    """Two servers' worth of canned answers: `send(base, method, path, headers, timeout, now)`."""

    def __init__(self, answers, events=None):
        self.answers, self.sent = answers, []
        self.events = [] if events is None else events   # every call in order, with resets when a test shares the list

    def __call__(self, base, method, path, headers, timeout, now=None, body=None):
        self.sent.append((base, path, timeout))
        self.events.append((base, method, path, headers["Authorization"].rsplit('Token="', 1)[-1].rstrip('"'), body))
        answer = self.answers[(base, path)]
        if isinstance(answer, list):   # successive answers to the same path; the last one repeats
            answer = answer.pop(0) if len(answer) > 1 else answer[0]
        return answer if isinstance(answer, diff.Response) else diff.Response(200, json.dumps(answer).encode(), 0.01)


def request(id="r1", trap=""):
    return diff.Request(id, "GET", f"/{id}", {}, None, "", trap, 200)


class RunRequests(Scratch):
    def run_one(self, oracle, ferrofin, entries=(), trap="", rules=()):
        servers = FakeServers({("o", "/r1"): oracle, ("f", "/r1"): ferrofin})
        results, used, cap, _, _ = diff.run_requests([request(trap=trap)], IDS, "o", "f", {"oracle": "t", "ferrofin": "t"}, list(rules),
                                               list(entries), 30, None, send=servers)
        return results[0], cap

    def test_identical_answers_match(self):
        result, _ = self.run_one({"A": 1}, {"A": 1})
        self.assertEqual((result["state"], result["differences"]), ("match", []))

    def test_a_difference_is_new_unless_the_ledger_has_it(self):
        result, _ = self.run_one({"A": 1}, {"A": 2})
        self.assertEqual(result["state"], "differences")
        self.assertIsNone(result["differences"][0]["ledger"])
        self.assertIn('"A": 1', result["bodies"]["oracle"])
        (entry,) = self.ledger('[[entry]]\nrequest = "r1"\npath = "A"\nissue = 7\nkind = "value"\nreason = "known"\n')
        result, _ = self.run_one({"A": 1}, {"A": 2}, [entry])
        self.assertEqual((result["state"], result["differences"][0]["ledger"], result["bodies"]), ("ledgered", "issue gl-7", None))

    def test_each_servers_own_token_in_its_answer_is_masked_before_comparing_and_storing(self):
        mine, theirs = "0123456789abcdef0123456789abcdef", "fedcba9876543210fedcba9876543210"
        servers = FakeServers({("o", "/r1"): diff.Response(200, f"a.ts?ApiKey={mine}\n".encode(), 0.1),
                               ("f", "/r1"): diff.Response(200, f"a.ts?ApiKey={theirs}\n".encode(), 0.1)})
        results, *_ = diff.run_requests([request()], IDS, "o", "f", {"oracle": mine, "ferrofin": theirs}, [], [], 30, None, send=servers)
        self.assertEqual(results[0]["state"], "match")
        servers = FakeServers({("o", "/r1"): diff.Response(200, f"ApiKey={mine}".encode(), 0.1),
                               ("f", "/r1"): diff.Response(200, b"ApiKey=other", 0.1)})
        results, *_ = diff.run_requests([request()], IDS, "o", "f", {"oracle": mine, "ferrofin": theirs}, [], [], 30, None, send=servers)
        self.assertEqual(results[0]["bodies"]["oracle"], "ApiKey=<token>")

    def test_an_opaque_whole_body_rule_accepts_two_different_bodies_that_are_not_json(self):
        rules = self.rules('[[rule]]\npaths = ["$"]\nkind = "opaque"\nrequests = ["r1"]\nreason = "encoder output"\n')
        result, _ = self.run_one(diff.Response(200, b"\x00jpeg-a", 0.1), diff.Response(200, b"\x00jpeg-b", 0.1), rules=rules)
        self.assertEqual(result["state"], "match")
        result, _ = self.run_one(diff.Response(200, b"\x00jpeg-a", 0.1), diff.Response(200, b"", 0.1), rules=rules)
        self.assertEqual(result["state"], "differences")

    def test_a_ledger_path_matches_every_image_tag_key_as_tag(self):
        (entry,) = self.ledger('[[entry]]\nrequest = "r1"\npath = "ImageBlurHashes.Primary.<tag>"\nissue = 7\n'
                               'kind = ["missing", "extra"]\nreason = "keyed by the tag"\n')
        result, _ = self.run_one({"ImageBlurHashes": {"Primary": {"0123456789abcdef0123456789abcdef": "h"}}},
                                 {"ImageBlurHashes": {"Primary": {"fedcba9876543210fedcba9876543210": "h"}}}, [entry])
        self.assertEqual((result["state"], {d["ledger"] for d in result["differences"]}), ("ledgered", {"issue gl-7"}))

    def test_one_new_difference_among_ledgered_ones_keeps_the_request_new(self):
        (entry,) = self.ledger('[[entry]]\nrequest = "r1"\npath = "A"\nissue = 7\nkind = "value"\nreason = "known"\n')
        result, _ = self.run_one({"A": 1, "B": 1}, {"A": 2, "B": 2}, [entry])
        self.assertEqual(result["state"], "differences")
        self.assertEqual(sorted(bool(d["ledger"]) for d in result["differences"]), [False, True])

    def test_a_different_status_is_a_difference_at_the_status(self):
        result, _ = self.run_one(diff.Response(200, b"{}", 0.1), diff.Response(404, b"{}", 0.1))
        self.assertEqual([(d["at"], d["kind"], d["oracle"], d["ferrofin"]) for d in result["differences"]], [("$status", "status", "200", "404")])

    def test_a_ferrofin_timeout_is_a_difference(self):
        result, _ = self.run_one({"A": 1}, diff.Response(None, b"", 30.0, "timeout"))
        self.assertEqual([(d["kind"], d["ferrofin"]) for d in result["differences"]], [("status", "timeout")])

    def test_two_servers_that_agree_on_an_error_are_no_pass_unless_the_request_expects_it(self):
        broken = lambda: diff.Response(500, b"", 0.1)
        result, _ = self.run_one(broken(), broken())
        self.assertEqual(result["state"], "differences")
        self.assertEqual([(d["at"], d["oracle"]) for d in result["differences"]], [("$status", "500 (expected 200)")])
        servers = FakeServers({("o", "/r1"): broken(), ("f", "/r1"): broken()})
        expecting = diff.Request("r1", "GET", "/r1", {}, None, "", "", 500)
        results, _, _, _, _ = diff.run_requests([expecting], IDS, "o", "f", {"oracle": "t", "ferrofin": "t"}, [], [], 30, None, send=servers)
        self.assertEqual(results[0]["state"], "match")

    def test_an_oracle_that_fails_instead_of_stalling_on_a_known_trap_is_an_oracle_failure(self):
        result, _ = self.run_one(diff.Response(503, b"", 0.1), {"A": 1}, trap="fails on large libraries")
        self.assertEqual((result["state"], result["differences"]), ("oracle failed", []))

    def test_a_request_the_cap_cut_short_is_no_ferrofin_difference_and_ends_the_run(self):
        servers = FakeServers({("o", "/r1"): diff.Response(None, b"", 5.0, "timeout"), ("f", "/r1"): {}})
        results, _, cap, _, _ = diff.run_requests([request()], IDS, "o", "f", {"oracle": "t", "ferrofin": "t"}, [], [], 30, 105.0,
                                            wall=lambda: 100.0, send=servers)
        self.assertEqual((results, cap), ([], True))
        servers = FakeServers({("o", "/r1"): diff.Response(None, b"", 30.0, "timeout"), ("f", "/r1"): {}})
        results, _, cap, _, _ = diff.run_requests([request()], IDS, "o", "f", {"oracle": "t", "ferrofin": "t"}, [], [], 30, 500.0,
                                            wall=lambda: 100.0, send=servers)
        self.assertEqual((len(results), cap), (1, False))   # a real timeout well inside the cap still counts

    def test_a_request_nobody_answered_is_a_difference_too(self):
        timeout = diff.Response(None, b"", 30.0, "timeout")
        result, _ = self.run_one(timeout, timeout)
        self.assertEqual(result["state"], "differences")

    def test_an_oracle_timeout_on_a_known_trap_is_an_oracle_failure_and_no_difference(self):
        result, _ = self.run_one(diff.Response(None, b"", 30.0, "timeout"), {"A": 1}, trap="locks the database")
        self.assertEqual((result["state"], result["differences"]), ("oracle failed", []))

    def test_an_oracle_timeout_on_any_other_request_is_a_difference(self):
        result, _ = self.run_one(diff.Response(None, b"", 30.0, "timeout"), {"A": 1})
        self.assertEqual(result["state"], "differences")

    def test_bodies_that_are_not_json_must_be_byte_equal(self):
        text = lambda b: diff.Response(200, b, 0.1)
        self.assertEqual(self.run_one(text(b"plain"), text(b"plain"))[0]["state"], "match")
        self.assertEqual(self.run_one(text(b"plain"), text(b"other"))[0]["state"], "differences")
        self.assertEqual(self.run_one(text(b"plain"), text(b"{}"))[0]["state"], "differences")

    def test_the_cap_stops_the_run_before_the_next_request_and_is_a_failure(self):
        servers = FakeServers({("o", "/r1"): {}, ("f", "/r1"): {}, ("o", "/r2"): {}, ("f", "/r2"): {}})
        clock = iter([100.0, 100.0, 200.0, 200.0])
        results, _, cap, _, _ = diff.run_requests([request("r1"), request("r2")], IDS, "o", "f", {"oracle": "t", "ferrofin": "t"}, [], [], 30, 150.0,
                                            wall=lambda: next(clock), send=servers)
        self.assertEqual(([r["id"] for r in results], cap), (["r1"], True))
        self.assertEqual(diff.verdict_of(results, cap)[0], "run failed")
        self.assertIn("cap exceeded", diff.verdict_of(results, cap)[1])

    def test_the_cap_passed_before_the_first_request_sends_nothing(self):
        servers = FakeServers({})
        results, _, cap, _, _ = diff.run_requests([request()], IDS, "o", "f", {"oracle": "t", "ferrofin": "t"}, [], [], 30, 10.0,
                                            wall=lambda: 20.0, send=servers)
        self.assertEqual((results, cap, servers.sent), ([], True, []))

    def test_each_call_of_a_pair_gets_what_is_left_of_the_cap_not_the_first_calls_share(self):
        servers = FakeServers({("o", "/r1"): {}, ("f", "/r1"): {}})
        clock = iter([100.0, 103.0, 104.0])   # the oracle call starts at 100, Ferrofin's at 103, the check after at 104
        diff.run_requests([request()], IDS, "o", "f", {"oracle": "t", "ferrofin": "t"}, [], [], 30, 105.0, wall=clock.__next__, send=servers)
        self.assertEqual([timeout for _, _, timeout in servers.sent], [5.0, 2.0])

    def test_when_the_oracle_call_spends_the_cap_ferrofin_is_not_called(self):
        servers = FakeServers({("o", "/r1"): {}, ("f", "/r1"): {}})
        clock = iter([100.0, 106.0, 106.0])
        results, _, cap, _, _ = diff.run_requests([request()], IDS, "o", "f", {"oracle": "t", "ferrofin": "t"}, [], [], 30, 105.0,
                                            wall=clock.__next__, send=servers)
        self.assertEqual(([url for url, _, _ in servers.sent], results, cap), (["o"], [], True))

    def test_a_run_that_finishes_every_request_exactly_at_the_cap_is_not_a_failure(self):
        servers = FakeServers({("o", "/r1"): {}, ("f", "/r1"): {}})
        results, _, cap, _, _ = diff.run_requests([request()], IDS, "o", "f", {"oracle": "t", "ferrofin": "t"}, [], [], 30, 100.0,
                                            wall=iter([50.0, 50.0, 100.0]).__next__, send=servers)
        self.assertEqual((len(results), cap), (1, False))


class WriteRuns(Scratch):
    def load(self, *lines):
        return diff.load_requests(write(self.dir, "w.jsonl", "\n".join(lines) + "\n"), IDS)

    def resetter(self, events, tokens=("o2", "f2"), error=None):
        calls = []

        def reset(timeout):
            calls.append(timeout)
            events.append(("reset", timeout))
            if error:
                raise error
            return {"oracle": tokens[0], "ferrofin": tokens[1]}
        return reset, calls

    def run_writes(self, requests, answers, reset, rules=(), deadline=None, wall=time.time, events=None, sleep=None):
        servers = FakeServers(answers, events)
        return diff.run_requests(requests, IDS, "o", "f", {"oracle": "t", "ferrofin": "t"}, list(rules), [], 30, deadline,
                                 wall=wall, send=servers, reset=reset, sleep=sleep or (lambda s: None))

    def test_a_settling_read_back_asks_a_server_again_until_its_refresh_filled_the_field(self):
        (request,) = self.load('{"id": "bs", "method": "POST", "path": "/c", "readbacks": [{"id": "d", "path": "/d", "settle": "ProductionYear"}]}')
        reset, _ = self.resetter([])
        events, naps = [], []
        answers = {("o", "/c"): {}, ("f", "/c"): {}, ("o", "/d"): {"Genres": [], "ProductionYear": 1953},
                   ("f", "/d"): [{"Genres": []}, {"Genres": [], "ProductionYear": None}, {"Genres": [], "ProductionYear": 1953}]}
        outcome = self.run_writes([request], answers, reset, events=events, sleep=naps.append)
        self.assertEqual([(r["id"], r["state"]) for r in outcome.results], [("bs", "match"), ("bs/d", "match")])
        self.assertEqual([e[:3] for e in events if e[2] == "/d"], [("o", "GET", "/d")] + [("f", "GET", "/d")] * 3)
        self.assertEqual(naps, [diff.SETTLE_POLL_S] * 2)

    def test_a_settling_poll_the_cap_cuts_short_ends_the_run_as_cap_exceeded(self):
        (request,) = self.load('{"id": "bs", "method": "POST", "path": "/c", "readbacks": [{"id": "d", "path": "/d", "settle": "Year"}]}')
        reset, _ = self.resetter([])

        def send(base, method, path, headers, timeout, now=None, body=None):
            if (base, path) == ("f", "/d") and timeout < 30:
                return diff.Response(None, b"", timeout, "timeout")
            return diff.Response(200, json.dumps({"Year": 1} if base == "o" or path != "/d" else {}).encode(), 0.01)

        clock = iter([0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 95.0, 95.0, 95.0, 95.0, 95.0, 95.0, 95.0, 95.0, 95.0, 95.0])
        outcome = diff.run_requests([request], IDS, "o", "f", {"oracle": "t", "ferrofin": "t"}, [], [], 30, 100.0,
                                    wall=lambda: next(clock, 95.0), send=send, reset=reset, sleep=lambda s: None)
        self.assertTrue(outcome.cap_exceeded)
        self.assertNotIn("bs/d", [r["id"] for r in outcome.results])

    def test_a_read_back_whose_field_never_comes_compares_the_last_answer(self):
        (request,) = self.load('{"id": "bs", "method": "POST", "path": "/c", "readbacks": [{"id": "d", "path": "/d", "settle": "Year"}]}')
        reset, _ = self.resetter([])
        clock = iter(range(0, 1000, 10))
        servers = FakeServers({("o", "/c"): {}, ("f", "/c"): {}, ("o", "/d"): {"Year": 1}, ("f", "/d"): {"N": 1}})
        outcome = diff.run_requests([request], IDS, "o", "f", {"oracle": "t", "ferrofin": "t"}, [], [], 30, None,
                                    now=lambda: float(next(clock)), send=servers, reset=reset, sleep=lambda s: None)
        self.assertEqual(outcome.results[1]["state"], "differences")
        self.assertLessEqual(sum(1 for base, path, _ in servers.sent if (base, path) == ("f", "/d")), 4)

    def test_reads_run_first_then_each_write_on_a_fresh_reset_followed_by_its_read_backs(self):
        requests = self.load('{"id": "w1", "method": "POST", "path": "/w1", "body": {"A": "{movie}"}, "readbacks": [{"id": "r", "path": "/r"}]}',
                             '{"id": "a", "method": "GET", "path": "/a"}',
                             '{"id": "w2", "method": "DELETE", "path": "/w2", "readbacks": [{"id": "r", "path": "/r"}]}')
        events = []
        reset, _ = self.resetter(events)
        answers = {(side, path): {} for side in "of" for path in ("/a", "/w1", "/w2", "/r")}
        outcome = self.run_writes(requests, answers, reset, events=events)
        self.assertEqual([r["id"] for r in outcome.results], ["a", "w1", "w1/r", "w2", "w2/r"])
        self.assertEqual([r["state"] for r in outcome.results], ["match"] * 5)
        order = [e[0] if e[0] == "reset" else f"{e[0]} {e[1]} {e[2]} {e[3]}" for e in events]
        self.assertEqual(order, ["o GET /a t", "f GET /a t", "reset",
                                 "o POST /w1 o2", "f POST /w1 f2", "o GET /r o2", "f GET /r f2", "reset",
                                 "o DELETE /w2 o2", "f DELETE /w2 f2", "o GET /r o2", "f GET /r f2"])
        self.assertEqual(json.loads(events[3][4]), {"A": "m1"})
        self.assertIsNone(events[8][4])
        writes = [r for r in outcome.results if r["method"] != "GET"]
        self.assertTrue(all(isinstance(r["reset_s"], float) for r in writes))
        self.assertEqual([r.get("of") for r in outcome.results], [None, None, "w1", None, "w2"])
        self.assertEqual((outcome.cap_exceeded, outcome.failure), (False, ""))

    def test_a_read_back_follows_the_id_each_server_gave_and_a_scoped_rule_accepts_the_new_ids(self):
        (request,) = self.load('{"id": "pl", "method": "POST", "path": "/p", "readbacks": [{"id": "items", "path": "/p/{write.Id}"}]}')
        rules = self.rules('[[rule]]\npaths = ["Id"]\nkind = "opaque"\nrequests = ["pl"]\nreason = "a new id on each server"\n')
        reset, _ = self.resetter([])
        answers = {("o", "/p"): {"Id": "po"}, ("f", "/p"): {"Id": "pf"}, ("o", "/p/po"): {"N": 1}, ("f", "/p/pf"): {"N": 1}}
        outcome = self.run_writes([request], answers, reset, rules)
        self.assertEqual([(r["id"], r["state"]) for r in outcome.results], [("pl", "match"), ("pl/items", "match")])
        self.assertEqual((outcome.results[1]["target"], outcome.results[1]["ferrofin_target"]), ("/p/po", "/p/pf"))

    def test_a_read_back_a_server_gave_nothing_to_capture_for_is_a_difference(self):
        (request,) = self.load('{"id": "pl", "method": "POST", "path": "/p", "readbacks": [{"id": "items", "path": "/p/{write.Id}"}]}')
        reset, _ = self.resetter([])
        answers = {("o", "/p"): {"Id": "po"}, ("f", "/p"): {}, ("o", "/p/po"): {"N": 1}}
        outcome = self.run_writes([request], answers, reset)
        readback = outcome.results[1]
        self.assertEqual(readback["state"], "differences")
        self.assertEqual([(d["at"], d["kind"], d["oracle"]) for d in readback["differences"]], [("$status", "status", "200")])
        self.assertIn("not sent: the write's answer has no `write.Id`", readback["differences"][0]["ferrofin"])

    def test_setup_steps_run_on_the_fresh_copy_before_the_write_and_are_compared(self):
        (request,) = self.load('{"id": "unfav", "method": "DELETE", "path": "/f", "setup": [{"id": "fav", "method": "POST", "path": "/f", "body": {"A": 1}}], '
                               '"readbacks": [{"id": "r", "path": "/r"}]}')
        events = []
        reset, _ = self.resetter(events)
        answers = {(side, path): {} for side in "of" for path in ("/f", "/r")}
        answers[("f", "/f")] = {"Odd": True}   # answered by both the setup POST and the DELETE
        outcome = self.run_writes([request], answers, reset, events=events)
        order = [e[0] if e[0] == "reset" else f"{e[0]} {e[1]} {e[2]}" for e in events]
        self.assertEqual(order, ["reset", "o POST /f", "f POST /f", "o DELETE /f", "f DELETE /f", "o GET /r", "f GET /r"])
        self.assertEqual([(r["id"], r["state"], r.get("of")) for r in outcome.results],
                         [("unfav/fav", "differences", "unfav"), ("unfav", "differences", None), ("unfav/r", "match", "unfav")])
        self.assertIn("reset_s", outcome.results[1])
        self.assertEqual(json.loads(events[1][4]), {"A": 1})

    def test_a_setup_step_the_oracle_fails_on_a_known_trap_skips_the_write_and_its_read_backs(self):
        (request,) = self.load('{"id": "w", "method": "DELETE", "path": "/f", "setup": [{"id": "s", "method": "POST", "path": "/s", "oracle_trap": "slow"}], '
                               '"readbacks": [{"id": "r", "path": "/r"}]}')
        events = []
        reset, _ = self.resetter(events)
        answers = {("o", "/s"): diff.Response(None, b"", 30.0, "timeout"), ("f", "/s"): {}}
        outcome = self.run_writes([request], answers, reset, events=events)
        self.assertEqual([(r["id"], r["state"]) for r in outcome.results], [("w/s", "oracle failed"), ("w", "oracle failed"), ("w/r", "oracle failed")])
        self.assertEqual([e[2] for e in events if e[0] != "reset"], ["/s", "/s"])
        self.assertIn("reset_s", outcome.results[1])

    def test_each_server_acts_on_what_its_own_setup_step_created(self):
        (request,) = self.load('{"id": "add", "method": "POST", "path": "/p/{setup.pl.Id}/items", "body": {"Of": "{setup.pl.Id}"}, '
                               '"setup": [{"id": "pl", "method": "POST", "path": "/p"}], "readbacks": [{"id": "r", "path": "/p/{setup.pl.Id}"}]}')
        events = []
        reset, _ = self.resetter(events)
        answers = {("o", "/p"): {"Id": "po"}, ("f", "/p"): {"Id": "pf"}, ("o", "/p/po/items"): {}, ("f", "/p/pf/items"): {},
                   ("o", "/p/po"): {}, ("f", "/p/pf"): {}}
        rules = self.rules('[[rule]]\npaths = ["Id"]\nkind = "opaque"\nrequests = ["add/pl"]\nreason = "r"\n')
        outcome = self.run_writes([request], answers, reset, rules, events=events)
        self.assertEqual([(r["id"], r["state"]) for r in outcome.results], [("add/pl", "match"), ("add", "match"), ("add/r", "match")])
        sent = [(e[0], e[2], e[4]) for e in events if e[0] != "reset"]
        self.assertEqual(sent[2:4], [("o", "/p/po/items", b'{"Of": "po"}'), ("f", "/p/pf/items", b'{"Of": "pf"}')])

    def test_a_trap_does_not_hide_a_read_back_a_server_was_not_asked(self):
        (request,) = self.load('{"id": "pl", "method": "POST", "path": "/p", "readbacks": [{"id": "items", "path": "/p/{write.Id}", "oracle_trap": "slow"}]}')
        reset, _ = self.resetter([])
        outcome = self.run_writes([request], {("o", "/p"): {}, ("f", "/p"): {"Id": "pf"}, ("f", "/p/pf"): {}}, reset)
        self.assertEqual(outcome.results[1]["state"], "differences")

    def test_a_write_the_oracle_fails_on_a_known_trap_skips_its_read_backs(self):
        (request,) = self.load('{"id": "bk", "method": "POST", "path": "/b", "oracle_trap": "locks the database", "readbacks": [{"id": "r", "path": "/r"}]}')
        events = []
        reset, _ = self.resetter(events)
        answers = {("o", "/b"): diff.Response(None, b"", 30.0, "timeout"), ("f", "/b"): {}}
        outcome = self.run_writes([request], answers, reset, events=events)
        self.assertEqual([(r["id"], r["state"]) for r in outcome.results], [("bk", "oracle failed"), ("bk/r", "oracle failed")])
        self.assertNotIn("/r", [e[2] for e in events if e[0] != "reset"])

    def test_a_reset_that_fails_fails_the_run_and_sends_nothing_more(self):
        requests = self.load('{"id": "w1", "method": "POST", "path": "/w1"}', '{"id": "w2", "method": "POST", "path": "/w2"}')
        events = []
        reset, _ = self.resetter(events, error=diff.ResetError("ferrofin: did not answer /System/Ping"))
        outcome = self.run_writes(requests, {}, reset, events=events)
        self.assertEqual((outcome.results, len(events)), ([], 1))
        self.assertIn("resetting the servers before `w1` failed: ferrofin: did not answer /System/Ping", outcome.failure)
        self.assertEqual(diff.verdict_of(outcome.results, outcome.cap_exceeded, outcome.failure)[0], "run failed")

    def test_a_reset_gets_what_is_left_of_the_cap_and_one_the_cap_cuts_short_is_cap_exceeded(self):
        (request,) = self.load('{"id": "w1", "method": "POST", "path": "/w1"}')
        reset, calls = self.resetter([], error=diff.ResetError("killed at the deadline", timed_out=True))
        outcome = self.run_writes([request], {}, reset, deadline=160.0, wall=lambda: 100.0)
        self.assertEqual((calls, outcome.cap_exceeded, outcome.failure), ([60.0], True, ""))

    def test_without_a_deadline_a_reset_is_unbounded_here_and_bounded_by_the_reset_script(self):
        (request,) = self.load('{"id": "w1", "method": "POST", "path": "/w1"}')
        reset, calls = self.resetter([])
        self.run_writes([request], {("o", "/w1"): {}, ("f", "/w1"): {}}, reset)
        self.assertEqual(calls, [None])

    def test_a_write_needs_a_reset(self):
        (request,) = self.load('{"id": "w1", "method": "POST", "path": "/w1"}')
        with self.assertRaisesRegex(diff.ParityError, "a write needs a fresh database copy"):
            self.run_writes([request], {}, None)


class Verdict(unittest.TestCase):
    def test_a_run_in_which_the_oracle_failed_on_every_request_compared_nothing_and_fails(self):
        failed = [{"differences": [], "state": "oracle failed"}]
        self.assertEqual(diff.verdict_of(failed, False)[0], "run failed")
        self.assertIn("no request was compared", diff.verdict_of(failed, False)[1])
        mixed = failed + [{"differences": [], "state": "match"}]
        self.assertEqual(diff.verdict_of(mixed, False)[0], "pass")
        self.assertEqual(diff.verdict_of([], False)[0], "pass")   # no requests at all is refused earlier, by load_requests

    def test_the_verdicts_and_their_exit_codes(self):
        clean = [{"differences": [], "state": "match"}]
        new = [{"differences": [{"ledger": None}], "state": "differences"}]
        ledgered = [{"differences": [{"ledger": "issue gl-1"}], "state": "ledgered"}]
        self.assertEqual(diff.verdict_of(clean, False)[0], "pass")
        self.assertEqual(diff.verdict_of(ledgered, False)[0], "pass")
        self.assertEqual(diff.verdict_of(new, False)[0], "new differences")
        self.assertEqual(diff.verdict_of(clean, True)[0], "run failed")
        self.assertEqual(diff.verdict_of(clean, False, "a server did not boot")[0], "run failed")
        self.assertEqual([diff.exit_code(v) for v in ("pass", "new differences", "run failed")], [0, 1, 1])


class Report(Scratch):
    def summary(self, results, **extra):
        verdict, reason = diff.verdict_of(results, False)
        return {"verdict": verdict, "reason": reason, "planned": len(results), "requests": results, "elapsed_s": 1.5,
                "run": {"name": "run1", "oracle": {"image": "jf:12.2"}, "ferrofin": {"image": "ff:test"}}, "request_timeout_s": 30,
                "cap_s": 7200, "unused_rules": [], "unused_ledger": [], **extra}

    def result(self, id, state, differences=(), bodies=None):
        return {"id": id, "method": "GET", "target": f"/{id}", "source": "", "state": state, "differences": list(differences),
                "oracle": {"status": 200, "seconds": 0.01, "error": ""}, "ferrofin": {"status": 200, "seconds": 0.02, "error": ""},
                "bodies": bodies}

    def difference(self, ledger=None, at="A.B"):
        return {"path": at, "at": at, "kind": "value", "oracle": "1", "ferrofin": "2", "ledger": ledger, "ledger_reason": None}

    def test_new_differences_come_first_and_both_responses_are_shown(self):
        page = diff.render(self.summary([
            self.result("ok", "match"),
            self.result("led", "ledgered", [self.difference("issue gl-9", "L.L")]),
            self.result("bad", "differences", [self.difference(None, "N.N")], {"oracle": '{"A": 1}', "ferrofin": '{"A": 2}'}),
        ]))
        self.assertLess(page.index("New differences"), page.index("Matched by the ledger"))
        self.assertLess(page.index("N.N"), page.index("issue gl-9"))
        self.assertIn("NEW DIFFERENCES", page)
        self.assertIn("&quot;A&quot;: 2", page)
        self.assertIn("Jellyfin response", page)
        self.assertIn("jf:12.2", page)
        self.assertIn("1.5 s of the 7200 s cap", page)

    def test_a_pass_says_so_and_lists_what_matched_nothing(self):
        page = diff.render(self.summary([self.result("ok", "match")], unused_rules=[{"paths": ["Gone.X"], "reason": "r"}],
                                        unused_ledger=[{"request": "a", "path": "P", "link": "issue gl-3"}]))
        self.assertIn(">PASS<", page)
        self.assertIn("Gone.X", page)
        self.assertIn("issue gl-3", page)

    def test_oracle_failures_are_listed_apart_with_the_trap_and_what_ferrofin_answered(self):
        failed = {**self.result("trap", "oracle failed"), "oracle_trap": "a plugin's route"}
        failed["oracle"] = {"status": None, "seconds": 30.0, "error": "timeout"}
        failed["ferrofin"] = {"status": 418, "seconds": 0.1, "error": ""}
        page = diff.render(self.summary([failed]))
        self.assertIn("Oracle failures (1)", page)
        self.assertIn("timeout", page)
        self.assertIn("a plugin&#x27;s route", page)
        self.assertIn("418", page)

    def test_the_timing_is_broken_down_by_request_file(self):
        results = [{**self.result("a", "match"), "file": "contract.jsonl"}, {**self.result("b", "match"), "file": "combos.jsonl"},
                   {**self.result("c", "match"), "file": "contract.jsonl", "reset_s": 4.5}]
        results.append({**self.result("d", "differences", [self.difference()], {"oracle": "{}", "ferrofin": "{}"}), "file": "edge.jsonl"})
        page = diff.render(self.summary(results))
        section = page[page.index("By request file"):]
        self.assertIn("contract.jsonl", section)
        self.assertIn("combos.jsonl", section)
        self.assertEqual(diff.time_by_file(results)["edge.jsonl"]["states"], {"differences": 1})
        self.assertEqual(diff.time_by_file(results)["contract.jsonl"]["states"], {"match": 2})

    def test_each_write_shows_its_reset_time_and_its_read_backs_and_the_resets_count_in_the_timing(self):
        write_result = {**self.result("fav", "match"), "method": "POST", "reset_s": 12.25}
        readbacks = [{**self.result("fav/data", "match"), "of": "fav"},
                     {**self.result("fav/list", "differences", [self.difference()], {"oracle": "{}", "ferrofin": "{}"}), "of": "fav"}]
        page = diff.render(self.summary([self.result("read", "match"), write_result, *readbacks]))
        section = page[page.index("Writes (1)"):]
        self.assertIn("12.2 s", section)
        self.assertIn("fav/data", section)
        self.assertIn("fav/list", section)
        self.assertIn("1 reset, 12.2 s in all", page)

    def test_a_large_body_is_kept_only_in_part(self):
        kept = diff.stored(b"x" * (diff.STORED_BODY_BYTES + 10))
        self.assertTrue(kept.startswith("x" * 100))
        self.assertIn(f"{diff.STORED_BODY_BYTES + 10} bytes in all", kept)
        self.assertEqual(diff.stored(b"small"), "small")

    def test_the_timing_of_each_phase_is_shown_against_the_cap(self):
        timing = {"reads": {"requests": 400, "seconds": 120.5, "concurrency": 4}, "writes": {"requests": 180, "seconds": 900.25, "resets_s": 800.0}}
        page = diff.render(self.summary([self.result("ok", "match")], timing=timing))
        self.assertIn("400 reads in 120.5 s, 4 at a time", page)
        self.assertIn("180 write requests in 900.2 s, 800.0 s of it resets", page)

    def test_the_text_of_a_response_cannot_inject_markup(self):
        page = diff.render(self.summary([self.result("bad", "differences", [self.difference()], {"oracle": "<script>x</script>", "ferrofin": "{}"})]))
        self.assertNotIn("<script>x", page)

    def test_the_page_is_self_contained(self):
        page = diff.render(self.summary([self.result("ok", "match")]))
        for external in ("src=", "href=", "http://", "https://", "@import"):
            self.assertNotIn(external, page)


UNSET = object()


class Overview(Scratch):
    """result.json: each request's outcome counted overall and by API area, for the session page and the README."""

    @staticmethod
    def summary():
        new = lambda path, kind="value", filed=None, ledger=None: {"path": path, "kind": kind, "ledger": ledger, "filed": filed}
        requests = [
            {"target": "/Items/1?Fields=Overview", "state": "match", "differences": []},
            {"target": "/Items/2", "state": "match", "differences": []},
            {"target": "/Items/3", "state": "ledgered", "differences": [new("Tags", ledger="ledger.toml#12")]},
            {"target": "/Years", "state": "ledgered", "differences": [new("Items", "order", ledger="ledger.toml#40")]},
            {"target": "/Years?limit=2", "state": "ledgered",
             "differences": [new("Items", "order", ledger="ledger.toml#40"), new("Items[].Name", ledger="ledger.toml#41")]},
            {"target": "Users/u1/Items", "state": "differences", "differences": [new("A"), new("B", ledger="ledger.toml#3")]},
            {"target": "/Videos/1/stream", "state": "differences", "differences": [new("C", filed=832), new("D", ledger="x")]},
            {"target": "/Videos/2/stream", "state": "differences", "differences": [new("E", filed=860), new("F")]},
            {"target": "/Intros", "state": "oracle failed", "differences": []}]
        return {"verdict": "new differences", "reason": "2 new difference(s)", "planned": 9, "requests": requests,
                "elapsed_s": 60.0, "run": {"started": 1792915910, "sha": "5112240e", "ids_sha256": "4b21",
                                           "oracle": {"version": "12.2.0", "image": "jellyfin@pin"},
                                           "ferrofin": {"image": "ferrofin:x", "image_id": "sha256:f"}}}

    def test_each_request_has_one_outcome_and_an_oracle_failure_is_never_identical(self):
        outcomes = [diff.outcome(r) for r in self.summary()["requests"]]
        self.assertEqual(outcomes, ["identical", "identical", "accepted", "order", "accepted", "new", "filed", "new",
                                    "oracle_failed"])

    def test_the_overview_counts_outcomes_overall_and_by_area_with_its_source(self):
        doc = diff.result_doc(self.summary())
        self.assertEqual(diff.gate_result().problems(doc), [])
        self.assertEqual((doc["gate"], doc["verdict"]), ("parity", "new differences"))
        self.assertEqual(doc["counts"], {"identical": 2, "order": 1, "accepted": 2, "filed": 1, "new": 2, "oracle_failed": 1})
        detail = doc["detail"]
        self.assertEqual(detail["areas"], [
            {"area": "/Items", "requests": 3, "identical": 2, "order": 0, "accepted": 1, "filed": 0, "new": 0, "oracle_failed": 0},
            {"area": "/Videos", "requests": 2, "identical": 0, "order": 0, "accepted": 0, "filed": 1, "new": 1, "oracle_failed": 0},
            {"area": "/Years", "requests": 2, "identical": 0, "order": 1, "accepted": 1, "filed": 0, "new": 0, "oracle_failed": 0},
            {"area": "/Intros", "requests": 1, "identical": 0, "order": 0, "accepted": 0, "filed": 0, "new": 0, "oracle_failed": 1},
            {"area": "/Users", "requests": 1, "identical": 0, "order": 0, "accepted": 0, "filed": 0, "new": 1, "oracle_failed": 0}])
        self.assertEqual((detail["sent"], detail["planned"], detail["oracle_version"]), (9, 9, "12.2.0"))
        self.assertEqual((detail["new_differences"], detail["unfiled_groups"], detail["filed_issues"]), (4, 2, [832, 860]))
        self.assertEqual({k: doc["source"][k] for k in ("commit", "image", "image_digest", "oracle", "fixture")},
                         {"commit": "5112240e", "image": "ferrofin:x", "image_digest": "sha256:f", "oracle": "jellyfin@pin",
                          "fixture": "4b21"})
        self.assertEqual((doc["started"], doc["ended"]), ("2026-10-25T08:11:50+00:00", "2026-10-25T08:12:50+00:00"))

    def test_the_result_command_writes_the_overview_of_a_run_from_before_it(self):
        out = self.dir / "run"
        out.mkdir()
        (out / "results.json").write_text(json.dumps(self.summary()))
        with contextlib.redirect_stdout(io.StringIO()) as printed:
            self.assertEqual(diff.main(["result", "--out", str(out)]), 0)
        self.assertEqual(printed.getvalue().strip(), str(out / "result.json"))
        again = diff.gate_result().read(str(out), "parity")
        self.assertEqual(again["counts"], diff.result_doc(self.summary())["counts"])
        self.assertIsNone(again["source"]["host_class"])   # not the host regenerating it
        (out / "results.json").write_text(json.dumps({"requests": [{"target": "/x", "state": "differences", "differences": [{}]}]}))
        with contextlib.redirect_stderr(io.StringIO()) as err:
            self.assertEqual(diff.main(["result", "--out", str(out)]), 2)   # a misshapen file is said, not a traceback
        self.assertIn("KeyError", err.getvalue())


class Handler(BaseHTTPRequestHandler):
    routes = {}
    login = UNSET   # what POST /Users/AuthenticateByName answers, instead of a token
    login_status = 200

    def log_message(self, *args):
        pass

    def respond(self, status, body, delay=0.0, drip=0.0):
        time.sleep(delay)
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        for index in range(len(data)):   # drip: one byte per `drip` seconds
            self.wfile.write(data[index:index + 1])
            self.wfile.flush()
            time.sleep(drip)
        if not drip:
            self.wfile.write(b"")

    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", 0)))
        if self.path == "/Users/AuthenticateByName":
            default = {"AccessToken": "tok-" + str(self.server.server_port)}
            self.respond(self.login_status, default if self.login is UNSET else self.login)
        else:
            self.respond(*self.routes.get("POST " + self.path, (404, {})))

    def do_DELETE(self):
        self.respond(*self.routes.get("DELETE " + self.path, (404, {})))

    def do_GET(self):
        route = self.routes.get(self.path)
        if route is None:
            self.respond(404, {})
        else:
            self.respond(*route)


@contextlib.contextmanager
def serve(routes, **attributes):
    handler = type("H", (Handler,), {"routes": routes, **attributes})
    server = type("Quiet", (ThreadingHTTPServer,), {"daemon_threads": True, "handle_error": lambda self, request, address: None})(("127.0.0.1", 0), handler)
    thread = threading.Thread(target=lambda: server.serve_forever(poll_interval=0.02), daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}"
    finally:
        server.shutdown()
        server.server_close()


class Live(Scratch):
    def test_fetch_gives_up_at_the_timeout_and_says_so(self):
        with serve({"/slow": (200, {}, 1.0)}) as base:
            started = time.monotonic()
            reply = diff.fetch(base, "GET", "/slow", {}, 0.3)
        self.assertEqual((reply.status, reply.error), (None, "timeout"))
        self.assertLess(time.monotonic() - started, 0.9)

    def test_fetch_cuts_off_a_server_that_trickles_its_body_at_the_total_timeout(self):
        with serve({"/drip": (200, {"A": "x" * 200}, 0.0, 0.1)}) as base:
            started = time.monotonic()
            reply = diff.fetch(base, "GET", "/drip", {}, 0.8)
        self.assertEqual((reply.status, reply.error), (None, "timeout"))
        self.assertLess(time.monotonic() - started, 2.0)

    def test_fetch_always_returns_a_response_even_for_a_request_line_it_cannot_send(self):
        with serve({}) as base:
            reply = diff.fetch(base, "GET", "/caf\u00e9", {}, 2)
        self.assertEqual(reply.status, None)
        self.assertTrue(reply.error.startswith("connection:"))

    def test_fetch_reports_a_refused_connection(self):
        with serve({}) as base:
            pass
        reply = diff.fetch(base, "GET", "/x", {}, 1)
        self.assertEqual(reply.status, None)
        self.assertTrue(reply.error.startswith("connection:"))

    def test_login_returns_the_access_token_and_refuses_a_server_that_gives_none(self):
        with serve({}) as base:
            self.assertTrue(diff.login(base, IDS, 5).startswith("tok-"))
        with serve({}) as base:
            pass   # closed again: nothing listens on this port
        with self.assertRaisesRegex(diff.ParityError, "cannot sign in"):
            diff.login(base, IDS, 1)
        for body in ([], "text", None, 5):   # an answer that is JSON but not an object
            with serve({}, login=body) as base, self.assertRaisesRegex(diff.ParityError, "no access token|not JSON"):
                diff.login(base, IDS, 5)
        with serve({}, login={"AccessToken": ""}) as base, self.assertRaisesRegex(diff.ParityError, "no access token"):
            diff.login(base, IDS, 5)
        with serve({}, login_status=401) as base, self.assertRaisesRegex(diff.ParityError, "answered 401"):
            diff.login(base, IDS, 5)

    def files(self, requests='{"id": "a", "method": "GET", "path": "/Items/{movie}"}\n'):
        return {"ids": write(self.dir, "ids.json", json.dumps(IDS)), "requests": write(self.dir, "r.jsonl", requests),
                "normalise": write(self.dir, "n.toml", "version = 1\n"), "ledger": write(self.dir, "l.toml", "version = 1\n")}

    def run_cli(self, oracle, ferrofin, files, *extra, out=None):
        out = out or str(self.dir / "out")
        argv = ["run", "--oracle-url", oracle, "--ferrofin-url", ferrofin, "--ids", files["ids"], "--requests", files["requests"],
                "--normalise", files["normalise"], "--ledger", files["ledger"], "--out", out, *extra]
        with contextlib.redirect_stdout(io.StringIO()):
            return diff.main(argv), Path(out)

    def test_a_whole_run_over_two_servers_that_agree_exits_zero_and_writes_the_report(self):
        with serve({"/Items/m1": (200, {"Id": "m1", "Name": "x"})}) as a, serve({"/Items/m1": (200, {"Id": "m1", "Name": "x"})}) as b:
            code, out = self.run_cli(a, b, self.files())
        self.assertEqual(code, 0)
        summary = json.loads((out / "results.json").read_text())
        self.assertEqual((summary["verdict"], summary["requests"][0]["state"]), ("pass", "match"))
        self.assertIn("PASS", (out / "report.html").read_text())
        result = diff.gate_result().read(str(out), "parity")   # its overview beside the report
        self.assertEqual((result["verdict"], result["counts"]["identical"], result["detail"]["areas"][0]["area"]),
                         ("pass", 1, "/Items"))
        self.assertIn(f"New differences ({result['detail']['new_differences']})", (out / "report.html").read_text())

    def test_a_whole_run_with_a_nulled_field_exits_one_and_names_the_field_path_in_the_report(self):
        sick = {"Id": "m1", "Name": "x", "MediaSources": [{"Size": 5, "Path": None}]}
        well = {"Id": "m1", "Name": "x", "MediaSources": [{"Size": 5, "Path": "/p"}]}
        with serve({"/Items/m1": (200, well)}) as a, serve({"/Items/m1": (200, sick)}) as b:
            code, out = self.run_cli(a, b, self.files())
        self.assertEqual(code, 1)
        page = (out / "report.html").read_text()
        self.assertIn("MediaSources[0].Path", page)
        self.assertIn("NEW DIFFERENCES", page)

    def test_unused_rules_and_ledger_entries_are_found_per_entry_even_when_two_share_a_link(self):
        files = self.files()
        files["normalise"] = write(self.dir, "n.toml", 'version = 1\n[[rule]]\npaths = ["Gone"]\nkind = "opaque"\nreason = "never applies"\n')
        files["ledger"] = write(self.dir, "l.toml", 'version = 1\n[[entry]]\nrequest = "a"\npath = "Name"\nkind = "value"\nissue = 4\nreason = "k"\n'
                                '[[entry]]\nrequest = "a"\npath = "Other"\nkind = "value"\nissue = 4\nreason = "stale"\n')
        with serve({"/Items/m1": (200, {"Name": "x"})}) as a, serve({"/Items/m1": (200, {"Name": "y"})}) as b:
            code, out = self.run_cli(a, b, files)
        summary = json.loads((out / "results.json").read_text())
        self.assertEqual(code, 0)
        self.assertEqual([u["path"] for u in summary["unused_ledger"]], ["Other"])
        self.assertEqual([u["paths"] for u in summary["unused_rules"]], [["Gone"]])

    def test_the_elapsed_time_counts_from_the_start_of_the_run_boot_included(self):
        out = self.dir / "out"
        out.mkdir()
        (out / "run.json").write_text(json.dumps({"started": time.time() - 100, "cap_s": 7200,
                                                  "oracle": {"boot_s": 7}, "ferrofin": {"boot_s": 9}}))
        with serve({"/Items/m1": (200, {})}) as a, serve({"/Items/m1": (200, {})}) as b:
            _, out = self.run_cli(a, b, self.files(), out=str(out))
        self.assertGreaterEqual(json.loads((out / "results.json").read_text())["elapsed_s"], 100)
        self.assertIn("Jellyfin 7 s / Ferrofin 9 s", (out / "report.html").read_text())

    def test_an_unresolvable_template_stops_the_run_before_the_first_request(self):
        files = self.files(requests='{"id": "a", "method": "GET", "path": "/Items/{nothing}"}\n')
        stderr = io.StringIO()
        with serve({}) as a, serve({}) as b, contextlib.redirect_stderr(stderr):
            code, out = self.run_cli(a, b, files)
        self.assertEqual(code, 2)
        self.assertIn("no `nothing`", stderr.getvalue())
        self.assertFalse(out.exists())

    def test_check_with_ids_catches_an_unresolvable_template(self):
        files = self.files(requests='{"id": "a", "method": "GET", "path": "/Items/{nothing}"}\n')
        argv = ["check", "--requests", files["requests"], "--normalise", files["normalise"], "--ledger", files["ledger"]]
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(diff.main(argv), 0)
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            self.assertEqual(diff.main([*argv, "--ids", files["ids"]]), 2)
        self.assertIn("no `nothing`", stderr.getvalue())

    def test_a_ledgered_difference_does_not_fail_the_run(self):
        files = self.files()
        files["ledger"] = write(self.dir, "l.toml", 'version = 1\n[[entry]]\nrequest = "a"\npath = "Name"\nissue = 4\nkind = "value"\nreason = "known"\n')
        with serve({"/Items/m1": (200, {"Name": "x"})}) as a, serve({"/Items/m1": (200, {"Name": "y"})}) as b:
            code, out = self.run_cli(a, b, files)
        self.assertEqual(code, 0)
        self.assertIn("issue gl-4", (out / "report.html").read_text())

    def test_an_invalid_ledger_stops_the_run_before_any_request(self):
        files = self.files()
        files["ledger"] = write(self.dir, "l.toml", 'version = 1\n[[entry]]\nrequest = "a"\npath = "Name"\nreason = "no link"\n')
        stderr = io.StringIO()
        with serve({}) as a, serve({}) as b, contextlib.redirect_stderr(stderr):
            code, out = self.run_cli(a, b, files)
        self.assertEqual(code, 2)
        self.assertIn("exactly one link", stderr.getvalue())
        self.assertFalse(out.exists())

    def test_the_request_timeout_is_held_to_its_range(self):
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            code, _ = self.run_cli("http://x", "http://y", self.files(), "--request-timeout", "9")
        self.assertEqual(code, 2)
        self.assertIn("10 to 120", stderr.getvalue())

    def test_a_deadline_already_passed_is_a_failed_run_with_a_report(self):
        with serve({"/Items/m1": (200, {})}) as a, serve({"/Items/m1": (200, {})}) as b:
            code, out = self.run_cli(a, b, self.files(), "--deadline", "1")
        self.assertEqual(code, 1)
        summary = json.loads((out / "results.json").read_text())
        self.assertEqual((summary["verdict"], summary["cap_exceeded"], summary["requests"]), ("run failed", True, []))
        self.assertIn("cap exceeded", (out / "report.html").read_text())

    def test_the_failure_command_writes_a_report_for_a_run_that_could_not_complete(self):
        out = self.dir / "failed"
        out.mkdir()
        (out / "run.json").write_text(json.dumps({"name": "failed-run", "cap_s": 7200}))
        with contextlib.redirect_stdout(io.StringIO()):
            code = diff.main(["failure", "--out", str(out), "--reason", "ferrofin did not start"])
        self.assertEqual(code, 1)
        page = (out / "report.html").read_text()
        self.assertIn("RUN FAILED", page)
        self.assertIn("ferrofin did not start", page)
        self.assertIn("failed-run", page)
        self.assertEqual(json.loads((out / "new-differences.json").read_text())["requests"], [])
        result = diff.gate_result().read(str(out), "parity")   # the overview of a run that compared nothing
        self.assertEqual((result["verdict"], result["detail"]["sent"], result["detail"]["reason"]),
                         ("run failed", 0, "ferrofin did not start"))

    def test_a_failure_report_is_written_even_when_run_json_is_empty_or_garbled(self):
        for content in ("", "{not json", "[1, 2]"):
            with self.subTest(content):
                out = self.dir / ("failed" + str(abs(hash(content))))
                out.mkdir()
                (out / "run.json").write_text(content)
                with contextlib.redirect_stdout(io.StringIO()):
                    self.assertEqual(diff.main(["failure", "--out", str(out), "--reason", "early failure"]), 1)
                self.assertIn("early failure", (out / "report.html").read_text())

    WRITE = '{"id": "fav", "method": "POST", "path": "/Fav/{movie}", "readbacks": [{"id": "data", "path": "/Items/{movie}"}]}\n'

    def test_a_whole_run_with_a_write_resets_the_servers_with_the_reset_command_and_signs_in_again(self):
        resets = self.dir / "resets"
        command = f"{sys.executable} -c \"import sys; open(sys.argv[1], 'a').write('reset\\n')\" {resets}"
        routes = {"POST /Fav/m1": (200, {"IsFavorite": True}), "/Items/m1": (200, {"Name": "x"})}
        with serve(routes) as a, serve(routes) as b:
            code, out = self.run_cli(a, b, self.files(self.WRITE), "--reset-command", command)
        self.assertEqual(code, 0)
        self.assertEqual(resets.read_text(), "reset\n")
        summary = json.loads((out / "results.json").read_text())
        self.assertEqual([(r["id"], r["state"]) for r in summary["requests"]], [("fav", "match"), ("fav/data", "match")])
        self.assertEqual((summary["resets"], summary["requests"][0]["reset_s"] >= 0), (1, True))

    def test_a_reset_command_that_fails_fails_the_run_with_its_message(self):
        command = f"{sys.executable} -c \"import sys; sys.exit('ferrofin: docker start failed')\""
        with serve({}) as a, serve({}) as b:
            code, out = self.run_cli(a, b, self.files(self.WRITE), "--reset-command", command)
        self.assertEqual(code, 1)
        summary = json.loads((out / "results.json").read_text())
        self.assertEqual(summary["verdict"], "run failed")
        self.assertIn("ferrofin: docker start failed", summary["reason"])

    def test_a_reset_command_that_overruns_the_cap_is_killed_with_what_it_started(self):
        marker = self.dir / "survivor"
        script = write(self.dir, "slow_reset.sh", f"#!/bin/sh\n(sleep 3; touch {marker}) &\nsleep 30\n")
        Path(script).chmod(0o755)
        started = time.monotonic()
        with serve({}) as a, serve({}) as b:
            code, out = self.run_cli(a, b, self.files(self.WRITE), "--reset-command", script, "--deadline", str(time.time() + 1.5))
        self.assertLess(time.monotonic() - started, 10)
        self.assertEqual((code, json.loads((out / "results.json").read_text())["cap_exceeded"]), (1, True))
        time.sleep(3)
        self.assertFalse(marker.exists())   # the reset's own children went with it

    def test_stopping_a_reset_command_when_none_is_running_does_nothing(self):
        diff.ResetCommand("true").stop()

    def test_writes_without_a_reset_command_are_refused_before_anything_is_sent(self):
        stderr = io.StringIO()
        with serve({}) as a, serve({}) as b, contextlib.redirect_stderr(stderr):
            code, out = self.run_cli(a, b, self.files(self.WRITE))
        self.assertEqual(code, 2)
        self.assertIn("--reset-command", stderr.getvalue())
        self.assertFalse(out.exists())

    def test_a_whole_run_takes_several_request_sets(self):
        files = self.files()
        second = write(self.dir, "edge.jsonl", '{"id": "e", "method": "GET", "path": "/Items/{movie}"}\n')
        with serve({"/Items/m1": (200, {"Id": "m1"})}) as a, serve({"/Items/m1": (200, {"Id": "m1"})}) as b:
            code, out = self.run_cli(a, b, files, "--requests", second)
        summary = json.loads((out / "results.json").read_text())
        self.assertEqual((code, [r["id"] for r in summary["requests"]]), (0, ["a", "e"]))

    def test_a_whole_run_discovers_names_from_the_oracle_and_records_the_timing(self):
        files = self.files('{"id": "g", "method": "GET", "path": "/Genres/{found.genre}"}\n')
        discover = write(self.dir, "d.toml", '[found.genre]\npath = "/Genres"\npick = "Items.0.Name"\n')
        routes = {"/Genres": (200, {"Items": [{"Name": "Drama"}]}), "/Genres/Drama": (200, {"Name": "Drama"})}
        with serve(routes) as a, serve(routes) as b:
            code, out = self.run_cli(a, b, files, "--discover", discover, "--concurrency", "2")
        summary = json.loads((out / "results.json").read_text())
        self.assertEqual((code, summary["requests"][0]["target"], summary["found"]), (0, "/Genres/Drama", {"genre": "Drama"}))
        self.assertEqual(summary["timing"]["reads"]["concurrency"], 2)

    def test_a_run_writes_its_new_differences_for_filing_with_normalised_bodies_and_the_issue_once_filed(self):
        files = self.files('{"id": "a", "method": "GET", "path": "/Items/{movie}"}\n{"id": "b", "method": "GET", "path": "/Items/{movie}/x"}\n')
        files["normalise"] = write(self.dir, "n.toml", 'version = 1\n[[rule]]\npaths = ["ServerId"]\nkind = "opaque"\nreason = "per server"\n')
        files["ledger"] = write(self.dir, "l.toml", 'version = 1\n[[entry]]\nrequest = "b"\npath = "Name"\nkind = "value"\nissue = 4\nreason = "known"\n')
        filed = write(self.dir, "f.toml", 'version = 1\n[[filed]]\nrequest = "a"\npath = "Count"\nkind = "value"\nissue = 830\n')
        oracle = {"/Items/m1": (200, {"ServerId": "s1", "Name": "x", "Count": 1, "Overview": "text"}), "/Items/m1/x": (200, {"Name": "x"})}
        ferrofin = {"/Items/m1": (200, {"ServerId": "s2", "Name": "x", "Count": 2, "Overview": None}), "/Items/m1/x": (200, {"Name": "y"})}
        with serve(oracle) as a, serve(ferrofin) as b:
            code, out = self.run_cli(a, b, files, "--filed", filed)
        self.assertEqual(code, 1)
        new = json.loads((out / "new-differences.json").read_text())
        (entry,) = new["requests"]   # b's only difference is ledgered: nothing new to file there
        self.assertEqual((entry["request"], entry["method"], entry["target"]), ("a", "GET", "/Items/m1"))
        self.assertEqual(sorted((d["path"], d["kind"], d["filed"]) for d in entry["differences"]),
                         [("Count", "value", 830), ("Overview", "null", None)])
        self.assertEqual(entry["bodies"]["oracle"]["ServerId"], "<opaque>")
        self.assertEqual(entry["bodies"]["ferrofin"]["Overview"], None)
        self.assertEqual(entry["rules_applied"], ["ServerId"])
        page = (out / "report.html").read_text()
        self.assertIn("gl-830", page)
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            self.assertEqual(diff.main(["findings", "--out", str(out)]), 0)
        text = stdout.getvalue()
        self.assertIn("`Overview` (null)", text)
        self.assertNotIn("`Count` (value)", text)   # filed already: not offered again
        self.assertIn("GET /Items/m1", text)
        summary = json.loads((out / "results.json").read_text())
        self.assertEqual(summary["unused_filed"], [])

    def test_a_filed_entry_that_matched_nothing_is_listed(self):
        files = self.files()
        filed = write(self.dir, "f.toml", 'version = 1\n[[filed]]\nrequest = "gone"\npath = "X"\nkind = "value"\nissue = 9\n')
        with serve({"/Items/m1": (200, {"A": 1})}) as a, serve({"/Items/m1": (200, {"A": 1})}) as b:
            _, out = self.run_cli(a, b, files, "--filed", filed)
        self.assertEqual(json.loads((out / "results.json").read_text())["unused_filed"], [{"request": "gone", "path": "X", "issue": 9}])
        self.assertIn("gl-9", (out / "report.html").read_text())

    def test_findings_say_when_a_body_is_cut_and_a_body_cannot_break_the_fence(self):
        body = {"Text": "```" + "x" * 5000}
        new = {"requests": [{"request": "a", "method": "GET", "target": "/x", "source": "", "rules_applied": [], "bodies": {"oracle": body, "ferrofin": {}},
                             "differences": [{"path": "Text", "at": "Text", "kind": "value", "oracle": "1", "ferrofin": "2", "filed": None}]}]}
        text = diff.findings(new)
        self.assertIn("cut at 4000 characters; the whole body is in new-differences.json", text)
        self.assertIn("````json", text)

    def test_findings_group_image_tag_keys_and_put_the_widest_group_first(self):
        entry = lambda id, diffs: {"request": id, "method": "GET", "target": "/x", "source": "", "rules_applied": [],
                                   "bodies": {"oracle": {}, "ferrofin": {}},
                                   "differences": [{"path": p, "at": p, "kind": k, "oracle": "1", "ferrofin": "2", "filed": None} for p, k in diffs]}
        new = {"requests": [entry("a", [("Hashes.Primary." + "a" * 32, "missing"), ("N", "value")]),
                            entry("b", [("Hashes.Primary." + "b" * 32, "missing")])]}
        text = diff.findings(new)
        self.assertIn("`Hashes.Primary.<tag>` (missing) in 2 request(s)", text)
        self.assertLess(text.index("Hashes.Primary.<tag>"), text.index("`N` (value)"))

    def test_check_validates_the_shipped_inputs(self):
        with contextlib.redirect_stdout(io.StringIO()) as out:
            code = diff.main(["check"])
        self.assertEqual(code, 0)
        self.assertIn("valid", out.getvalue())


if __name__ == "__main__":
    unittest.main()
