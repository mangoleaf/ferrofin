#!/usr/bin/env python3
"""A scripted stand-in for the endpoints verify/scan-behaviour.sh calls, for its bats tests.

It does not scan anything. Each request that would start a scan (a library or item refresh,
a webhook report) takes the next entry of a scenario file and applies it: the /metrics counters
it advances and the item fields it changes. So a scenario is the log of what a well-behaved
server would do, row by row, and a test that edits one entry makes one row fail.

    fake_server.py SCRATCH_DIR LIBRARY_ROOT SCENARIO_JSON TOKEN

It prints "port N" once it listens. Items are the .mkv files under SCRATCH_DIR, looked up by
path; their DTOs start with the fields the script reads and keep whatever the script posts.
Every request line is appended to requests.log beside the scenario. A request without TOKEN
in its Authorization header gets a 401.

A scenario entry (every key optional):
    {"created": 3, "updated": 0, "unchanged": 5, "removed": 0, "probes": 3, "requests": 0,
     "patch": {"Alpha": {"Overview": "..."}},  # fields set on the item whose path has "Alpha"
                                               # ("SERIES": on the scratch series)
     "saver": "relative/path",                 # a file the "server" writes under SCRATCH_DIR
     "hold": true,                             # the scan runs until the next scan request
     "snapshot": "DIR"}                        # copies every .nfo under SCRATCH_DIR into DIR
Past the end of the scenario, a scan completes and changes nothing.

The library is a movies library, or a tvshows one when FAKE_KIND=tvshows. In a TV library the
scratch folder is a series: FAKE_SERIES (a JSON object) puts one with those fields there before
the run, as a server that already scanned the folder would have; otherwise one appears (named
after the folder, no provider ids) once the folder holds an episode. FAKE_FETCHERS=on or off
saves the library's metadata-fetcher choice for every kind of its items (TheMovieDb ticked, or
none); FAKE_TYPE_OPTIONS (a JSON list) saves exactly that; unset, the library saved none.
"""

import hashlib
import http.server
import json
import os
import shutil
import sys
import threading
import urllib.parse

SCRATCH, LIBRARY_ROOT, SCENARIO, TOKEN = sys.argv[1:5]
LIBRARY_ID = "f137a2dd21bbc1b99aa5c0f6bf02a805"
KIND = os.environ.get("FAKE_KIND", "movies")
SERIES = json.loads(os.environ["FAKE_SERIES"]) if os.environ.get("FAKE_SERIES") else None
SERIES_ID = "5e1e5e1e5e1e5e1e5e1e5e1e5e1e5e1e"
FETCHERS = os.environ.get("FAKE_FETCHERS")
KINDS = ["Movie"] if KIND == "movies" else ["Series", "Season", "Episode"]
if os.environ.get("FAKE_TYPE_OPTIONS"):
    TYPE_OPTIONS = json.loads(os.environ["FAKE_TYPE_OPTIONS"])
elif FETCHERS is None:
    TYPE_OPTIONS = []
else:
    TYPE_OPTIONS = [
        {"Type": k, "MetadataFetchers": ["TheMovieDb"] if FETCHERS == "on" else []} for k in KINDS
    ]
with open(SCENARIO, encoding="utf-8") as f:
    STEPS = json.load(f)
LOG = os.path.join(os.path.dirname(SCENARIO), "requests.log")

lock = threading.Lock()
counters = {("ferrofin_library_scans_total", "api", "completed"): 0}
dtos = {SERIES_ID: {"Id": SERIES_ID, "Path": SCRATCH, **SERIES}} if SERIES else {}
held = []


def bump(key, n=1):
    counters[key] = counters.get(key, 0) + n


def finish_scan(trigger):
    bump(("ferrofin_library_scans_total", trigger, "completed"))


def scan(trigger):
    """Applies the next scenario entry as one scan by `trigger`."""
    while held:
        finish_scan(held.pop())
    step = STEPS.pop(0) if STEPS else {}
    for outcome in ("created", "updated", "unchanged", "removed"):
        bump(("ferrofin_library_scan_items_total", trigger, outcome), step.get(outcome, 0))
    bump(("ferrofin_media_probe_total", "ok"), step.get("probes", 0))
    bump(("ferrofin_metadata_provider_requests_total", "tmdb", "ok"), step.get("requests", 0))
    for needle, fields in step.get("patch", {}).items():
        if needle == "SERIES":
            series().update(fields)
            continue
        for path in videos():
            if needle in path:
                dto(item_id(path), path).update(fields)
    if "snapshot" in step:
        os.makedirs(step["snapshot"], exist_ok=True)
        for root, _, files in os.walk(SCRATCH):
            for name in files:
                if name.endswith(".nfo"):
                    shutil.copy(os.path.join(root, name), os.path.join(step["snapshot"], name))
    if "saver" in step:
        with open(os.path.join(SCRATCH, step["saver"]), "w", encoding="utf-8") as f:
            f.write("written by the server\n")
    if step.get("hold"):
        held.append(trigger)
    else:
        finish_scan(trigger)


def exposition():
    lines = [f"ferrofin_library_scan_in_progress{{otel_scope_name=\"ferrofin\"}} {len(held)}"]
    for key, value in sorted(counters.items()):
        name = key[0]
        if name == "ferrofin_library_scans_total":
            labels = f'result="{key[2]}",trigger="{key[1]}"'
        elif name == "ferrofin_library_scan_items_total":
            labels = f'outcome="{key[2]}",trigger="{key[1]}"'
        elif name == "ferrofin_media_probe_total":
            labels = f'result="{key[1]}"'
        else:
            labels = f'provider="{key[1]}",result="{key[2]}"'
        lines.append(f'{name}{{otel_scope_name="ferrofin",{labels}}} {value}')
    return "\n".join(lines) + "\n"


def videos():
    found = []
    for root, _, files in os.walk(SCRATCH):
        found += [os.path.join(root, f) for f in files if f.endswith(".mkv")]
    return sorted(found)


def series():
    """The scratch series' DTO, made on first use."""
    if SERIES_ID not in dtos:
        dtos[SERIES_ID] = {"Id": SERIES_ID, "Path": SCRATCH, "Name": os.path.basename(SCRATCH),
                           "ProviderIds": {}}
    return dtos[SERIES_ID]


def item_id(path):
    return hashlib.md5(path.encode()).hexdigest()


def dto(ident, path=None):
    if ident not in dtos:
        name = os.path.splitext(os.path.basename(path or ident))[0]
        dtos[ident] = {
            "Id": ident, "Name": name, "Path": path, "Overview": "Written by the verify script.",
            "CommunityRating": None, "OfficialRating": "PG", "LockData": False, "LockedFields": [],
            "ImageTags": {},
            "RemoteTrailers": [{"Url": "https://www.youtube.com/watch?v=x"}],
        }
    return dtos[ident]


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, body=None, content_type="application/json"):
        data = b"" if body is None else (body if isinstance(body, bytes) else json.dumps(body).encode())
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def route(self, method):
        with open(LOG, "a", encoding="utf-8") as f:
            f.write(f"{method} {self.path}\n")
        path = urllib.parse.urlparse(self.path).path
        if method == "GET" and path == "/metrics":
            with lock:
                return self.reply(200, exposition().encode(), "text/plain; version=0.0.4")
        if f'Token="{TOKEN}"' not in self.headers.get("Authorization", ""):
            return self.reply(401)
        length = int(self.headers.get("Content-Length") or 0)
        body = json.loads(self.rfile.read(length) or b"null") if length else None
        with lock:
            if method == "GET" and path == "/Users/Me":
                return self.reply(200, {"Name": "admin", "Policy": {"IsAdministrator": True}})
            if method == "GET" and path == "/System/Configuration":
                return self.reply(200, {"LibraryMonitorDelay": 0})
            if method == "GET" and path == "/Library/VirtualFolders":
                return self.reply(200, [{
                    "Name": "Lib", "ItemId": LIBRARY_ID, "CollectionType": KIND,
                    "Locations": [LIBRARY_ROOT],
                    "LibraryOptions": {"EnableRealtimeMonitor": False, "TypeOptions": TYPE_OPTIONS},
                }])
            query = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
            if method == "GET" and path == "/Items" and query.get("IncludeItemTypes") == ["Series"]:
                there = SERIES or (KIND == "tvshows" and videos())
                items = [{"Id": SERIES_ID, "Path": SCRATCH}] if there else []
                if there:
                    series()
                return self.reply(200, {"Items": items, "TotalRecordCount": len(items)})
            if method == "GET" and path == "/Items":
                items = [{"Id": item_id(p), "Path": p, "Name": dto(item_id(p), p)["Name"]} for p in videos()]
                return self.reply(200, {"Items": items, "TotalRecordCount": len(items)})
            parts = path.strip("/").split("/")
            if parts[0] == "Items" and len(parts) == 3 and parts[2] == "Refresh" and method == "POST":
                scan("api")
                return self.reply(204)
            if method == "POST" and path == "/Library/Media/Updated":
                scan("webhook")
                return self.reply(204)
            if parts[0] == "Items" and len(parts) == 2 and parts[1] in dtos:
                if method == "GET":
                    return self.reply(200, dtos[parts[1]])
                dtos[parts[1]] = body
                return self.reply(204)
        return self.reply(404)

    def do_GET(self):
        self.route("GET")

    def do_POST(self):
        self.route("POST")


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
print(f"port {server.server_address[1]}", flush=True)
server.serve_forever()
