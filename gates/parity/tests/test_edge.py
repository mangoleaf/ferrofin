#!/usr/bin/env python3
"""Self-tests of the curated edge-case set (gates/parity/requests/edge/): every category of behaviour B7 has
requests, and they are a valid request set. Stdlib only.

    python3 -m unittest discover -s gates/parity/tests
"""

import json
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import diff  # noqa: E402

EDGE = diff.HERE / "requests" / "edge"
CONTRACT = diff.ROOT / "contracts" / "jellyfin-openapi-10.11.8.json"
IDS = {"user": "u", "user_name": "n", "password": "p", "viewer": "v", "movie": "m", "hdr_movie": "h", "series": "s",
       "season": "se", "episode": "e", "views": {"movies": "vm", "tvshows": "vt", "music": "vu"},
       "counts": {"movies": 3001}}


def requests():
    return diff.load_requests(EDGE, diff.placeholder_ids(IDS, diff.load_discover(diff.HERE / "discover.toml")))


def every(rows):
    return [r for request in rows for r in (request, *request.setup, *request.readbacks)]


class EdgeSet(unittest.TestCase):
    def test_every_category_of_b7_has_requests(self):
        ids = [r.id for r in every(requests())]
        for category in ("edge-parental-rating", "edge-blocked-tags", "edge-paging-first", "edge-paging-last", "edge-paging-past-end",
                         "edge-sort-", "edge-fields-all", "edge-userdata-off", "edge-image-limit-0", "edge-image-limit-1",
                         "edge-image-limit-many", "edge-non-null-"):
            with self.subTest(category):
                self.assertTrue(any(i.startswith(category) for i in ids), f"no request for {category}")

    def test_the_restricted_users_are_asked_about_by_id_in_every_read_back(self):
        for request in requests():
            if request.id in ("edge-parental-rating", "edge-blocked-tags"):
                self.assertTrue(request.readbacks)
                for readback in request.readbacks:
                    with self.subTest(readback.id):
                        self.assertIn("{setup.user.Id}", json.dumps([readback.path, readback.query]))

    def test_each_category_sends_what_it_claims(self):
        by_id = {r.id: r for r in every(requests())}
        query = lambda id: by_id[id].query
        self.assertEqual([query(f"edge-paging-{n}")["startIndex"] for n in ("first", "last", "at-end", "past-end")],
                         [0, "{counts.movies-1}", "{counts.movies}", 100000])
        self.assertEqual([query(f"edge-image-limit-{n}")["imageTypeLimit"] for n in ("0", "1", "many")], [0, 1, 10])
        self.assertIs(query("edge-userdata-off")["enableUserData"], False)
        self.assertEqual(by_id["edge-parental-rating"].body["MaxParentalRating"], 13)
        self.assertEqual(by_id["edge-blocked-tags"].body["BlockedTags"], ["{found.tag}"])

    def test_each_restriction_is_seen_to_hide_the_movie_it_must(self):
        by_id = {r.id: r for r in every(requests())}
        for read, movie in (("edge-parental-rating/hidden", "{found.r_movie}"), ("edge-blocked-tags/hidden", "{found.tagged_movie}")):
            with self.subTest(read):
                self.assertIn(movie, by_id[read].path)
                self.assertNotEqual(by_id[read].expect_status, 200)

    def test_fields_all_asks_for_every_documented_field(self):
        contract = json.loads(CONTRACT.read_text())
        fields = contract["components"]["schemas"]["ItemFields"]["enum"]
        (request,) = [r for r in requests() if r.id == "edge-fields-all"]
        self.assertEqual(sorted(request.query["fields"]), sorted(fields))


if __name__ == "__main__":
    unittest.main()
