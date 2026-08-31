#!/usr/bin/env python3
"""The lab fixture server: an endless MPEG-TS broadcast, plus a plugin repository.

A tuner stream never ends, and both servers treat a source that does end as a dead
channel (Jellyfin deletes its live-stream buffer the moment the HTTP body completes), so
serving the clip as a static file will not do. This loops `loop.ts` forever on every GET,
writing it at roughly its real bitrate, one transport packet boundary at a time, for any
number of concurrent clients (the two servers plus their recorders).

  python3 livetv-source.py /media/synth/livetv/loop.ts <clip-seconds> [port]

Runs inside the `livetv-source` compose service (python:3-alpine, stdlib only); the clip
length comes from docker-compose.yml, next to where gen-fixtures.sh's 60 s is noted.

It also serves two FIXED plugin-repository manifests at /manifest.json and
/manifest-b.json, plus a deliberately POISONED one at /manifest-poison.json whose
content changes on every request (see below). `GET /Packages` aggregates whatever the configured repositories
publish, so diffing it against the live repo.jellyfin.org would make the row
depend on upstream content that changes without notice; pointing BOTH servers at
a manifest on this container instead makes the catalogue deterministic and
network-independent. The fixture is shaped to exercise the parts of
`InstallationManager.GetPackages`/`GetAvailablePackages` the row exists to prove:
no server-stamped repositoryName/repositoryUrl keys (no real manifest has them),
a version whose targetAbi is above 10.11.8, a version whose targetAbi does not
parse, and one package listed by BOTH manifests so the same-guid merge runs.
"""
import http.server
import itertools
import json
import os
import socketserver
import sys
import time

CLIP = sys.argv[1]
CLIP_SECONDS = float(sys.argv[2])
PORT = int(sys.argv[3]) if len(sys.argv) > 3 else 8000
CHUNK = 188 * 64           # a whole number of TS packets per write
CLIENT_TIMEOUT_S = 30      # a client that stops reading (killed container) is dropped


# The plugin-repository fixture. Keys match a real repo.jellyfin.org manifest
# exactly — in particular NEITHER "repositoryName" NOR "repositoryUrl" appears,
# because the server stamps those after the fetch
# (`InstallationManager.GetPackages`: `ver.RepositoryName = manifestName;`). A
# manifest that carried them would not test the deserialization path any client
# actually hits.
MANIFEST_A = [
    {
        "category": "Metadata",
        "guid": "9c4e63f1-031b-4f25-988b-4f7d78a8b53e",
        "name": "Parity Bookshelf",
        "description": "Book metadata for the parity fixture.",
        "overview": "Book metadata.",
        "owner": "parity",
        "imageUrl": "http://livetv-source:8000/bookshelf.png",
        "versions": [
            # Above 10.11.8: `GetPackages` removes it. Its presence proves the
            # ABI filter runs; its absence from the response is the assertion.
            {"version": "3.0.0.0", "targetAbi": "10.11.9.0", "changelog": "too new",
             "sourceUrl": "http://livetv-source:8000/a.zip", "checksum": "aaa",
             "timestamp": "2025-03-01T00:00:00Z"},
            # Built for EXACTLY this release. `ApplicationVersion` is an assembly
            # version, so 10.11.8 is (10,11,8,0) and this is compatible — the
            # boundary case the 10.11.8 oracle lists 9 real plugin versions at.
            {"version": "2.5.0.0", "targetAbi": "10.11.8.0", "changelog": "exact release",
             "sourceUrl": "http://livetv-source:8000/f.zip", "checksum": "fff",
             "timestamp": "2025-02-15T00:00:00Z"},
            {"version": "2.0.0.0", "targetAbi": "10.11.0.0", "changelog": "ok",
             "sourceUrl": "http://livetv-source:8000/b.zip", "checksum": "bbb",
             "timestamp": "2025-02-01T00:00:00Z"},
            # Unparseable targetAbi falls back to Version(0,0,0,1) and is KEPT.
            {"version": "1.0.0.0", "targetAbi": "not-a-version", "changelog": "kept",
             "sourceUrl": "http://livetv-source:8000/c.zip", "checksum": "ccc",
             "timestamp": "2025-01-01T00:00:00Z"},
        ],
    },
    {
        # Every version too new ⇒ the whole package is dropped
        # ("Don't add a package that doesn't have any compatible versions").
        "category": "General",
        "guid": "1f0e3dad-9990-4b2b-8d0e-3dad99904b2b",
        "name": "Parity TooNew",
        "description": "Never listed.",
        "overview": "Never listed.",
        "owner": "parity",
        "versions": [
            {"version": "9.0.0.0", "targetAbi": "12.0.0.0",
             "sourceUrl": "http://livetv-source:8000/d.zip", "checksum": "ddd"},
        ],
    },
]

# Manifest B lists the SAME guid as manifest A's first package, with a version
# that slots between A's two survivors — so the same-guid merge (`MergeSortedList`)
# is exercised, not just asserted about.
MANIFEST_B = [
    {
        "category": "Metadata",
        "guid": "9c4e63f1-031b-4f25-988b-4f7d78a8b53e",
        "name": "Parity Bookshelf",
        "description": "The SECOND repository's copy — the first repository wins.",
        "overview": "Second copy.",
        "owner": "parity-b",
        "versions": [
            {"version": "1.5.0.0", "targetAbi": "10.11.0.0", "changelog": "from repo B",
             "sourceUrl": "http://livetv-source:8000/e.zip", "checksum": "eee",
             "timestamp": "2025-01-15T00:00:00Z"},
        ],
    },
]

# The DISABLED repository's manifest — a poison pill, not a dead URL.
#
# `GetAvailablePackages` skips a repository with `Enabled: false` BEFORE fetching
# it (`if (repository.Enabled && repository.Url is not null)`). A dead URL cannot
# test that: a server that ignored the flag would get an instant 404, warn, skip,
# and produce a byte-identical catalogue — the leg would pass either way. This
# path is served, and every response is DIFFERENT: the package identity carries a
# per-request counter. So if either server fetches it the catalogues diverge (one
# has an extra package, or both have one with different guids/names), and the
# body diff goes red. If neither fetches it, nothing is added and the diff is
# clean — which is the only outcome the flag permits.
POISON_COUNTER = itertools.count(1)


def poison_manifest():
    n = next(POISON_COUNTER)
    return [
        {
            "category": "Metadata",
            # A fresh guid per request, so two servers that both fetched would
            # still disagree with each other, not just with the expected list.
            "guid": f"deadbeef-0000-0000-0000-{n:012d}",
            "name": f"Parity Poison {n}",
            "description": "A DISABLED repository must never be fetched.",
            "overview": "If this appears in /Packages, repository.Enabled was ignored.",
            "owner": "parity",
            "versions": [
                # targetAbi below the server version, so nothing else would drop it.
                {"version": f"{n}.0.0.0", "targetAbi": "10.0.0.0", "changelog": "poison",
                 "sourceUrl": "http://livetv-source:8000/poison.zip", "checksum": "ppp",
                 "timestamp": "2025-01-01T00:00:00Z"},
            ],
        },
    ]


MANIFESTS = {"/manifest.json": lambda: MANIFEST_A,
             "/manifest-b.json": lambda: MANIFEST_B,
             "/manifest-poison.json": poison_manifest}


class Broadcast(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    timeout = CLIENT_TIMEOUT_S

    def do_GET(self):   # noqa: N802 — BaseHTTPRequestHandler's naming
        manifest = MANIFESTS.get(self.path.split("?")[0])
        if manifest is not None:
            body = json.dumps(manifest()).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-cache")
            self.end_headers()
            self.wfile.write(body)
            return
        if not self.path.startswith("/live"):
            self.send_error(404)
            return
        size = os.path.getsize(CLIP)
        pace = CLIP_SECONDS / max(size / CHUNK, 1)   # seconds per chunk at the clip's bitrate
        self.send_response(200)
        self.send_header("Content-Type", "video/mp2t")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")       # close-delimited body: it never ends
        self.end_headers()
        next_at = time.monotonic()
        try:
            while True:
                with open(CLIP, "rb") as f:
                    while chunk := f.read(CHUNK):
                        self.wfile.write(chunk)
                        self.wfile.flush()
                        next_at += pace                 # a schedule, so sleeps do not drift
                        time.sleep(max(0.0, next_at - time.monotonic()))
        except OSError:
            pass   # the client hung up or stalled — the broadcast goes on for the others

    def log_message(self, *_):   # quiet: the suite's logs are the servers'
        pass


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


if __name__ == "__main__":
    Server(("0.0.0.0", PORT), Broadcast).serve_forever()
