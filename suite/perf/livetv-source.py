#!/usr/bin/env python3
"""The Live TV fixture's broadcast: an endless, real-time-paced MPEG-TS over HTTP.

A tuner stream never ends, and both servers treat a source that does end as a dead
channel (Jellyfin deletes its live-stream buffer the moment the HTTP body completes), so
serving the clip as a static file will not do. This loops `loop.ts` forever on every GET,
writing it at roughly its real bitrate, one transport packet boundary at a time, for any
number of concurrent clients (the two servers plus their recorders).

  python3 livetv-source.py /media/synth/livetv/loop.ts <clip-seconds> [port]

Runs inside the `livetv-source` compose service (python:3-alpine, stdlib only); the clip
length comes from docker-compose.yml, next to where gen-fixtures.sh's 60 s is noted.
"""
import http.server
import os
import socketserver
import sys
import time

CLIP = sys.argv[1]
CLIP_SECONDS = float(sys.argv[2])
PORT = int(sys.argv[3]) if len(sys.argv) > 3 else 8000
CHUNK = 188 * 64           # a whole number of TS packets per write
CLIENT_TIMEOUT_S = 30      # a client that stops reading (killed container) is dropped


class Broadcast(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    timeout = CLIENT_TIMEOUT_S

    def do_GET(self):   # noqa: N802 — BaseHTTPRequestHandler's naming
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
