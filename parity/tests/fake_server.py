#!/usr/bin/env python3
"""A stand-in for one Jellyfin or Ferrofin server, for the runner's own tests (stdlib only).

    fake_server.py PORT BEHAVIOUR

It answers what parity/run.sh asks while booting (/System/Ping, /System/Info/Public, the sign-in,
/Plugins, /ScheduledTasks, /Library/VirtualFolders) and one item, /Users/u1/Items/m1. BEHAVIOUR:
    same     the item is {"Id": "m1", "Name": "Movie", "Overview": "text"}
    nulled   the same, with Overview null
    slow     the item answers after 60 seconds
    quiet    nothing answers (a server that never becomes ready)
    leaky    the scheduled task keeps a trigger however it is told to drop them
"""

import json
import os
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

VERSION = os.environ.get("FAKE_VERSION", "12.2.0")


class Handler(BaseHTTPRequestHandler):
    behaviour = "same"

    def log_message(self, *args):
        pass

    def send(self, status, body):
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.behaviour == "quiet":
            time.sleep(3600)
        routes = {
            "/System/Ping": lambda: b'"Jellyfin Server"',
            "/System/Info/Public": lambda: {"Version": VERSION},
            "/Plugins": lambda: [],
            "/ScheduledTasks": lambda: [{"Id": "t1", "Name": "Task", "State": "Idle", "Triggers": [{"Type": "StartupTrigger"}] if self.behaviour == "leaky" else []}],
            "/Library/VirtualFolders": lambda: [{"LibraryOptions": {"EnableRealtimeMonitor": False, "AutomaticRefreshIntervalDays": 0,
                                                                    "TypeOptions": [{"MetadataFetchers": [], "ImageFetchers": []}]}}],
        }
        if self.path in routes:
            return self.send(200, routes[self.path]())
        if self.path == "/Users/u1/Items/m1":
            if self.behaviour == "slow":
                time.sleep(60)
            return self.send(200, {"Id": "m1", "Name": "Movie", "Overview": None if self.behaviour == "nulled" else "text"})
        return self.send(404, {})

    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", 0)))
        if self.path == "/Users/AuthenticateByName":
            return self.send(200, {"AccessToken": "token"})
        return self.send(204, b"")


if __name__ == "__main__":
    Handler.behaviour = sys.argv[2]
    server = ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler)
    server.daemon_threads = True
    server.serve_forever()
