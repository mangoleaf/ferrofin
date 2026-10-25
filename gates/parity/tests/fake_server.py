#!/usr/bin/env python3
"""A stand-in for one Jellyfin or Ferrofin server, for the runner's own tests (stdlib only).

    fake_server.py PORT BEHAVIOUR [CONFIG_DIR]

It answers what gates/parity/run.sh asks while booting (/System/Ping, /System/Info/Public, the sign-in,
/Plugins, /ScheduledTasks, /Library/VirtualFolders), one item, /Users/u1/Items/m1, and one write:
POST /UserFavoriteItems/<id> adds <id> to the favourites kept in CONFIG_DIR (the container's /config),
which GET /favourites lists, so a write survives a restart unless the configuration is put back. Every
request is logged to $FAKE_STATE/requests.log. BEHAVIOUR:
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
from pathlib import Path
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

VERSION = os.environ.get("FAKE_VERSION", "12.2.0")
CONFIG = Path(sys.argv[3]) if len(sys.argv) > 3 else None


def favourites():
    path = CONFIG / "favourites.json"
    return json.loads(path.read_text()) if path.exists() else []


def log(handler):
    if os.environ.get("FAKE_STATE"):
        with open(Path(os.environ["FAKE_STATE"]) / "requests.log", "a") as out:
            out.write(f"{handler.server.server_port} {handler.command} {handler.path}\n")


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
        log(self)
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
        if self.path == "/favourites":
            return self.send(200, sorted(favourites()))
        if self.path == "/Users/u1/Items/m1":
            if self.behaviour == "slow":
                time.sleep(60)
            return self.send(200, {"Id": "m1", "Name": "Movie", "Overview": None if self.behaviour == "nulled" else "text"})
        return self.send(404, {})

    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", 0)))
        log(self)
        if self.path == "/Users/AuthenticateByName":
            return self.send(200, {"AccessToken": "token"})
        if self.path.startswith("/UserFavoriteItems/"):
            item = self.path.rsplit("/", 1)[1]
            (CONFIG / "favourites.json").write_text(json.dumps(favourites() + [item]))
            return self.send(200, {"IsFavorite": True, "ItemId": item})
        return self.send(204, b"")


if __name__ == "__main__":
    Handler.behaviour = sys.argv[2]
    server = ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler)
    server.daemon_threads = True
    server.serve_forever()
