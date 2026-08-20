#!/usr/bin/env python3
"""Self-test for benchlib's pure fixture helpers.

Covers the rules a live run cannot cheaply re-check:

  1. `find_playlist` reuses the EXISTING bench playlist. Creating one
     unconditionally made every ctx refresh add another playlist to a
     long-lived fast-loop database, so the item totals the /Items rows measure
     drifted upward run over run.
  2. Every row in `endpoints.py` renders to a placeholder-free path from one
     complete context — a row templating on a fixture nobody resolves is a
     silent 0%-ok row, not a measurement.
  3. `first_name` URL-quotes what it returns: an unquoted "'Weird Al' Yankovic"
     is not a usable vegeta target.

Run: python3 suite/perf/benchlib_selftest.py   (exit 0 = green). No test
framework by design, matching the other suite self-tests.
"""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import benchlib  # noqa: E402
import endpoints  # noqa: E402

# ── find_playlist: reuse, never re-create ────────────────────────────────────
items = [{"Id": "a", "Name": "someone-elses"}, {"Id": "b", "Name": benchlib.PLAYLIST_NAME}]
assert benchlib.find_playlist(items, benchlib.PLAYLIST_NAME) == "b", "must find the bench playlist"
assert benchlib.find_playlist(items, "nope") == "", "absent name yields '' (create path)"
assert benchlib.find_playlist([], benchlib.PLAYLIST_NAME) == "", "empty library yields ''"
# First wins, so a database that somehow holds two keeps resolving to the same one.
twice = [{"Id": "x", "Name": benchlib.PLAYLIST_NAME}, {"Id": "y", "Name": benchlib.PLAYLIST_NAME}]
assert benchlib.find_playlist(twice, benchlib.PLAYLIST_NAME) == "x", "pick must be stable"

# ── every row's template is fillable from a complete context ─────────────────
CTX = {
    "userId": "U", "itemId": "I", "imageItemId": "M", "writeItemId": "W",
    "seriesId": "S", "playlistId": "P", "taskId": "T", "imageTag": "G",
    "genreName": "Action", "studioName": "A24", "personName": "A.%20J.%20Langer",
    "token": "tok", "username": "bench", "password": "pw",
}
for e in endpoints.ENDPOINTS:
    try:
        path = benchlib.render_path(e, CTX)
        if e["body"] is not None:
            benchlib.render_body(e["body"], CTX)
    except KeyError as missing:
        sys.exit(f"endpoint {e['name']!r} templates on {missing}, which no fixture supplies")
    assert "{" not in path, f"endpoint {e['name']!r} left a placeholder in {path!r}"

# ── first_name quotes what it returns ────────────────────────────────────────
# Stub the transport, not the helper: what is under test is the quoting inside
# first_name. Asserting against a pre-quoted CTX literal instead would pass even
# with the quoting deleted (checked — it did).
_real_get_json = benchlib.get_json
try:
    page = {"Items": [{"Name": "'Weird Al' Yankovic", "Id": "p1"}]}
    benchlib.get_json = lambda *a, **k: page
    got = benchlib.first_name("http://x", {"userId": "U", "token": "t"}, "Persons")
    assert got == "%27Weird%20Al%27%20Yankovic", f"name must arrive URL-quoted, got {got!r}"
    rendered = benchlib.render_path(endpoints.BY_NAME["person_detail"],
                                    dict(CTX, personName=got))
    assert " " not in rendered and "'" not in rendered, \
        f"rendered target must be a valid URL, got {rendered!r}"

    benchlib.get_json = lambda *a, **k: {"Items": []}
    assert benchlib.first_name("http://x", {"userId": "U", "token": "t"}, "Persons") == "", \
        "a library with no such facet yields '' (404s identically on both servers)"
    benchlib.get_json = lambda *a, **k: None
    assert benchlib.first_name("http://x", {"userId": "U", "token": "t"}, "Persons") == "", \
        "a failed probe yields '' rather than raising mid-bring-up"
finally:
    benchlib.get_json = _real_get_json

print("benchlib self-test: all assertions passed")
