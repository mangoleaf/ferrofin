#!/usr/bin/env python3
"""Self-tests of parity/diff.py: request loading, normalisation, comparison, the ledger, the cap and
the report, over fixture response pairs and two throwaway local HTTP servers. Stdlib only.

    python3 -m unittest discover -s parity/tests
"""

import contextlib
import io
import json
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
            "is not sent yet": ['{"id": "a", "method": "POST", "path": "/x"}'],
            "starts with a slash": ['{"id": "a", "method": "GET", "path": "x"}'],
            "`query` is an object": ['{"id": "a", "method": "GET", "path": "/x", "query": [1]}'],
            "carries no body": ['{"id": "a", "method": "GET", "path": "/x", "body": {}}'],
            "`expect_status` is an HTTP status code": ['{"id": "a", "method": "GET", "path": "/x", "expect_status": "200"}',],
        }
        for message, lines in cases.items():
            with self.subTest(message), self.assertRaisesRegex(diff.ParityError, message):
                self.load(*lines)
        with self.assertRaisesRegex(diff.ParityError, "no requests"):
            self.load("")


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


class Ledger(Scratch):
    def test_the_ledger_shipped_with_the_harness_is_valid(self):
        diff.load_ledger(diff.HERE / "ledger.toml")

    def test_an_entry_links_to_a_verified_row_or_an_issue(self):
        entries = self.ledger('[[entry]]\nrequest = "a"\npath = "X"\nverified = "GET /Shows/NextUp"\nkind = "value"\nreason = "r"\n'
                              '[[entry]]\nrequest = "b"\npath = "Y"\nissue = 12\nkind = "null"\nreason = "r"\n')
        self.assertEqual([e.link() for e in entries], ["VERIFIED GET /Shows/NextUp", "issue gl-12"])

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

    def __init__(self, answers):
        self.answers, self.sent = answers, []

    def __call__(self, base, method, path, headers, timeout, now=None):
        self.sent.append((base, path, timeout))
        answer = self.answers[(base, path)]
        return answer if isinstance(answer, diff.Response) else diff.Response(200, json.dumps(answer).encode(), 0.01)


def request(id="r1", trap=""):
    return diff.Request(id, "GET", f"/{id}", {}, None, "", trap, 200)


class RunRequests(Scratch):
    def run_one(self, oracle, ferrofin, entries=(), trap="", rules=()):
        servers = FakeServers({("o", "/r1"): oracle, ("f", "/r1"): ferrofin})
        results, used, cap = diff.run_requests([request(trap=trap)], IDS, "o", "f", {"oracle": "t", "ferrofin": "t"}, list(rules),
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
        results, _, _ = diff.run_requests([expecting], IDS, "o", "f", {"oracle": "t", "ferrofin": "t"}, [], [], 30, None, send=servers)
        self.assertEqual(results[0]["state"], "match")

    def test_an_oracle_that_fails_instead_of_stalling_on_a_known_trap_is_an_oracle_failure(self):
        result, _ = self.run_one(diff.Response(503, b"", 0.1), {"A": 1}, trap="fails on large libraries")
        self.assertEqual((result["state"], result["differences"]), ("oracle failed", []))

    def test_a_request_the_cap_cut_short_is_no_ferrofin_difference_and_ends_the_run(self):
        servers = FakeServers({("o", "/r1"): diff.Response(None, b"", 5.0, "timeout"), ("f", "/r1"): {}})
        results, _, cap = diff.run_requests([request()], IDS, "o", "f", {"oracle": "t", "ferrofin": "t"}, [], [], 30, 105.0,
                                            wall=lambda: 100.0, send=servers)
        self.assertEqual((results, cap), ([], True))
        servers = FakeServers({("o", "/r1"): diff.Response(None, b"", 30.0, "timeout"), ("f", "/r1"): {}})
        results, _, cap = diff.run_requests([request()], IDS, "o", "f", {"oracle": "t", "ferrofin": "t"}, [], [], 30, 500.0,
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
        results, _, cap = diff.run_requests([request("r1"), request("r2")], IDS, "o", "f", {"oracle": "t", "ferrofin": "t"}, [], [], 30, 150.0,
                                            wall=lambda: next(clock), send=servers)
        self.assertEqual(([r["id"] for r in results], cap), (["r1"], True))
        self.assertEqual(diff.verdict_of(results, cap)[0], "run failed")
        self.assertIn("cap exceeded", diff.verdict_of(results, cap)[1])

    def test_the_cap_passed_before_the_first_request_sends_nothing(self):
        servers = FakeServers({})
        results, _, cap = diff.run_requests([request()], IDS, "o", "f", {"oracle": "t", "ferrofin": "t"}, [], [], 30, 10.0,
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
        results, _, cap = diff.run_requests([request()], IDS, "o", "f", {"oracle": "t", "ferrofin": "t"}, [], [], 30, 105.0,
                                            wall=clock.__next__, send=servers)
        self.assertEqual(([url for url, _, _ in servers.sent], results, cap), (["o"], [], True))

    def test_a_run_that_finishes_every_request_exactly_at_the_cap_is_not_a_failure(self):
        servers = FakeServers({("o", "/r1"): {}, ("f", "/r1"): {}})
        results, _, cap = diff.run_requests([request()], IDS, "o", "f", {"oracle": "t", "ferrofin": "t"}, [], [], 30, 100.0,
                                            wall=iter([50.0, 50.0, 100.0]).__next__, send=servers)
        self.assertEqual((len(results), cap), (1, False))


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

    def test_oracle_failures_are_listed_apart(self):
        failed = self.result("trap", "oracle failed")
        failed["oracle"] = {"status": None, "seconds": 30.0, "error": "timeout"}
        page = diff.render(self.summary([failed]))
        self.assertIn("Oracle failures (1)", page)
        self.assertIn("timeout", page)

    def test_the_text_of_a_response_cannot_inject_markup(self):
        page = diff.render(self.summary([self.result("bad", "differences", [self.difference()], {"oracle": "<script>x</script>", "ferrofin": "{}"})]))
        self.assertNotIn("<script>x", page)

    def test_the_page_is_self_contained(self):
        page = diff.render(self.summary([self.result("ok", "match")]))
        for external in ("src=", "href=", "http://", "https://", "@import"):
            self.assertNotIn(external, page)


UNSET = object()


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
            self.respond(404, {})

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

    def test_a_failure_report_is_written_even_when_run_json_is_empty_or_garbled(self):
        for content in ("", "{not json", "[1, 2]"):
            with self.subTest(content):
                out = self.dir / ("failed" + str(abs(hash(content))))
                out.mkdir()
                (out / "run.json").write_text(content)
                with contextlib.redirect_stdout(io.StringIO()):
                    self.assertEqual(diff.main(["failure", "--out", str(out), "--reason", "early failure"]), 1)
                self.assertIn("early failure", (out / "report.html").read_text())

    def test_check_validates_the_shipped_inputs(self):
        with contextlib.redirect_stdout(io.StringIO()) as out:
            code = diff.main(["check"])
        self.assertEqual(code, 0)
        self.assertIn("valid", out.getvalue())


if __name__ == "__main__":
    unittest.main()
