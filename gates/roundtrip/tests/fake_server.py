#!/usr/bin/env python3
"""A stand-in for the server of a round-trip copy, for the harness's own tests (stdlib only): the little of the
Jellyfin API the stages use, keeping the user data it is sent.

test_usage.py serves a Stub in process. roundtrip.bats's fake docker starts one per container:

    fake_server.py copy DIR                               a data directory: data/jellyfin.db with an API key and an
                                                          administrator with a web session
    fake_server.py serve PORT_FILE ROLE CONFIG BEHAVIOUR  serve on a free loopback port, written to PORT_FILE once
                                                          listening; the state lives in CONFIG/fake-state.json, so a
                                                          copy of CONFIG answers as its source did. BEHAVIOUR "ok";
                                                          "start-failed": every request 503, as Jellyfin's setup server
                                                          answers once its start failed; "loses": the copy's first
                                                          favorite is gone, a return that lost a write; "exits": the
                                                          server stops (status 3) as soon as its port is written. ROLE
                                                          names the product.
"""

from contextlib import closing
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import sqlite3
import sys
import threading
from urllib.parse import parse_qs, urlsplit

sys.path.append(str(Path(__file__).resolve().parent.parent))
import answers  # noqa: E402

ADMIN = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
MOVIES = [f"{n:032x}" for n in range(1, 8)]   # seven movies; the first one is already played
STATE = "fake-state.json"


class Stub:
    """fail: a path prefix answered 500. drop: a path prefix answered 200 but not kept (a write that was lost)."""

    def __init__(self, fail="", drop="", state=None, product="Jellyfin Server", behaviour="ok"):
        self.fail, self.drop, self.state, self.product, self.behaviour = fail, drop, state, product, behaviour
        self.calls, self.lock = [], threading.Lock()
        saved = json.loads(state.read_text()) if state and state.exists() else {}
        self.refreshes = saved.get("refreshes", 0)
        self.users = saved.get("users", {"synthetic-admin": ADMIN})
        self.data = saved.get("data", {m: {"Played": m == MOVIES[0], "IsFavorite": False, "PlaybackPositionTicks": 0}
                                      for m in MOVIES})
        self.collections = saved.get("collections", [])   # [type, name, id, item count]
        if behaviour == "loses":
            self.data[MOVIES[0]]["IsFavorite"] = False

    def answer(self, method, path, query, body):
        with self.lock:
            self.calls.append((method, path))
            if self.behaviour == "start-failed":
                return 503, None
            if self.fail and path.startswith(self.fail):
                return 500, None
            if method == "POST":
                result = (200, {}) if self.drop and path.startswith(self.drop) else self.post(path, query, body)
                self.save()
                return result
            return self.get(path, query)

    def get(self, path, query):
        if path == "/System/Info/Public":
            return 200, {"Version": "12.4.0", "ProductName": self.product}
        if path == "/Users":
            return 200, [{"Name": name, "Id": key} for name, key in self.users.items()]
        if path == "/ScheduledTasks":
            return 200, [{"Key": "RefreshLibrary", "State": "Idle",
                          "LastExecutionResult": {"Status": "Completed", "EndTimeUtc": str(self.refreshes)}}]
        if path == "/Items" and "ParentId" in query or path.startswith("/Playlists/"):
            key = query["ParentId"][0] if "ParentId" in query else path.split("/")[2]
            found = [c for c in self.collections if c[2] == key]
            return (200, {"Items": [], "TotalRecordCount": found[0][3]}) if found else (404, None)
        if path == "/Items" and query.get("IncludeItemTypes") == ["Movie"]:
            return 200, {"Items": [self.movie(m) for m in MOVIES]}
        if path == "/Items" and query.get("IncludeItemTypes") == ["Playlist,BoxSet"]:
            return 200, {"Items": [{"Id": key, "Type": kind, "Name": name}
                                   for kind, name, key, _ in sorted(self.collections, key=lambda c: c[1])]}
        if path == "/Items" and "Filters" in query:
            wanted = {"IsPlayed": "Played", "IsFavorite": "IsFavorite", "IsResumable": "PlaybackPositionTicks"}
            key = wanted[query["Filters"][0]]
            mine = query.get("userId") == [ADMIN]
            return 200, {"Items": [self.movie(m) for m in MOVIES if mine and self.data[m][key]]}
        return 200, {}

    def post(self, path, query, body):
        parts = path.strip("/").split("/")
        if parts[0] == "UserPlayedItems":
            self.data[parts[1]]["Played"] = True
        elif parts[0] == "UserFavoriteItems":
            self.data[parts[1]]["IsFavorite"] = True
        elif parts[0] == "UserItems":
            self.data[parts[1]]["PlaybackPositionTicks"] = body["PlaybackPositionTicks"]
        elif path == "/Playlists":
            key = f"{len(self.collections) + 100:032x}"
            self.collections.append(["Playlist", body["Name"], key, len(body["Ids"])])
            return 200, {"Id": key}
        elif path == "/Collections":
            key = f"{len(self.collections) + 100:032x}"
            self.collections.append(["BoxSet", query["name"][0], key, len(query["ids"][0].split(","))])
            return 200, {"Id": key}
        elif path == "/Users/New":
            self.users[body["Name"]] = f"{len(self.users):032x}"
        elif path == "/Library/Refresh":
            self.refreshes += 1
            return 204, None
        elif path == "/Users/AuthenticateByName":
            ok = body["Username"] in self.users and answers.GATE_USERS.get(body["Username"]) == body["Pw"]
            return (200, {"AccessToken": "token"}) if ok else (401, None)
        return 200, {}

    def movie(self, key):
        return {"Id": key, "Name": f"Movie {key[-1]}", "RunTimeTicks": 600_000_000, "UserData": dict(self.data[key])}

    def save(self):
        if self.state:
            self.state.write_text(json.dumps({"refreshes": self.refreshes, "users": self.users, "data": self.data,
                                              "collections": self.collections}))


def server(stub):
    """A ThreadingHTTPServer on a free loopback port answering through `stub`, not yet serving."""
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def handle_one(self, method):
            url = urlsplit(self.path)
            length = int(self.headers.get("Content-Length") or 0)
            body = json.loads(self.rfile.read(length)) if length else None
            status, result = stub.answer(method, url.path, parse_qs(url.query), body)
            raw = b"" if result is None else json.dumps(result).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

        def do_GET(self):
            self.handle_one("GET")

        def do_POST(self):
            self.handle_one("POST")

    return ThreadingHTTPServer(("127.0.0.1", 0), Handler)


def copy(root):
    """A data directory holding the tables the stages read: an API key and an administrator with a web session."""
    (root / "data").mkdir(parents=True)
    with closing(sqlite3.connect(root / "data" / "jellyfin.db")) as db:
        db.executescript("""
            CREATE TABLE ApiKeys (AccessToken TEXT, DateCreated TEXT);
            CREATE TABLE Users (Id TEXT, Username TEXT);
            CREATE TABLE Permissions (UserId TEXT, Kind INTEGER, Value INTEGER);
            CREATE TABLE Devices (UserId TEXT, AccessToken TEXT, DeviceId TEXT, AppName TEXT, DateLastActivity TEXT);
            INSERT INTO ApiKeys VALUES ('apikey', '2026-01-01');
            INSERT INTO Users VALUES ('AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA', 'synthetic-admin');
            INSERT INTO Permissions VALUES ('AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA', 0, 1);
            INSERT INTO Devices VALUES ('AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA', 'usertoken', 'device', 'Jellyfin Web', '2026-01-02');
        """)
        db.commit()
    return root


def main(argv):
    if len(argv) == 2 and argv[0] == "copy":
        copy(Path(argv[1]))
        return 0
    if len(argv) == 5 and argv[0] == "serve":
        port_file, role, config, behaviour = argv[1:]
        product = "Jellyfin Server" if role == "jellyfin" else "Ferrofin"
        httpd = server(Stub(state=Path(config) / STATE, product=product, behaviour=behaviour))
        partial = Path(f"{port_file}.partial")
        partial.write_text(str(httpd.server_port))
        partial.replace(port_file)
        if behaviour == "exits":
            return 3
        httpd.serve_forever()
        return 0
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
