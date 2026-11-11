#!/usr/bin/env python3
"""A stand-in for the server of a round-trip copy, for the harness's own tests (stdlib only): the little of the
Jellyfin API the stages use, keeping what it is sent.

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

import base64
import binascii
from contextlib import closing
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import re
import sqlite3
import sys
import threading
from urllib.parse import parse_qs, urlsplit

sys.path.append(str(Path(__file__).resolve().parent.parent))
import answers  # noqa: E402

ADMIN = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
MOVIES = [f"{n:032x}" for n in range(1, 8)]   # seven movies; the first one is already played
STATE = "fake-state.json"
#: What the stub keeps, and saves to CONFIG/fake-state.json.
FIELDS = ("refreshes", "users", "data", "collections", "accounts", "prefs", "keys", "devices", "libraries", "items",
          "images", "plays")


def fresh():
    return {
        "refreshes": 0,
        "users": {"synthetic-admin": ADMIN},
        "data": {m: {"Played": m == MOVIES[0], "IsFavorite": False, "PlaybackPositionTicks": 0, "PlayCount": 0,
                     "Likes": None} for m in MOVIES},
        "collections": [],   # [type, name, id, [[entry id, movie id], ...]]
        "accounts": {},      # user id: {"Policy": {...}, "Configuration": {...}}
        "prefs": {},         # user id: display preferences
        "keys": ["Synthetic adoption"],
        "devices": {},       # device id: custom name
        "libraries": [["Synthetic Movies", "movies", "1" * 32], ["Synthetic Photos", "homevideos", "2" * 32]],
        "items": {m: {"Name": f"Movie {m[-1]}", "LockData": False, "LockedFields": [], "Overview": "x"} for m in MOVIES},
        "images": {},        # item id: image types
        "plays": {},         # user id (not ADMIN): movies played to the end
    }


class Stub:
    """fail: a path prefix answered 500. drop: a path prefix (or, ending in $, a whole path; after "POST " or
    "DELETE ", that method's only) answered 200 but not kept (a write that was lost)."""

    def __init__(self, fail="", drop="", state=None, product="Jellyfin Server", behaviour="ok"):
        self.fail, self.drop, self.state, self.product, self.behaviour = fail, drop, state, product, behaviour
        self.calls, self.lock = [], threading.Lock()
        saved = json.loads(state.read_text()) if state and state.exists() else {}
        for field, value in fresh().items():
            setattr(self, field, saved.get(field, value))
        if behaviour == "loses":
            self.data[MOVIES[0]]["IsFavorite"] = False

    def answer(self, method, path, query, body, auth=""):
        with self.lock:
            self.calls.append((method, path))
            if self.behaviour == "start-failed":
                return 503, None
            if self.fail and path.startswith(self.fail):
                return 500, None
            if method in ("POST", "DELETE") and path.startswith("/Playlists/") and 'Token="apikey"' in auth:
                return 400, None   # upstream: GetUserById(Guid.Empty), as an API key has no user
            if method in ("POST", "DELETE"):
                verb, _, target = self.drop.rpartition(" ")
                lost = verb in ("", method) and (path == target[:-1] if target.endswith("$") else path.startswith(target))
                kept = not (self.drop and lost)
                result = (self.post if method == "POST" else self.delete)(path, query, body, auth) if kept else (200, {})
                self.save()
                return result
            return self.get(path, query)

    def user(self, key):
        account = self.accounts.get(key, {})
        name = next(n for n, k in self.users.items() if k == key)
        return {"Name": name, "Id": key,
                "Policy": {"IsAdministrator": key == ADMIN, "EnableAllFolders": True, **account.get("Policy", {})},
                "Configuration": {"SubtitleMode": "Default", **account.get("Configuration", {})}}

    def collection(self, key):
        return next((c for c in self.collections if c[2] == key), None)

    def get(self, path, query):
        parts = path.strip("/").split("/")
        if path == "/System/Info/Public":
            return 200, {"Version": "12.4.0", "ProductName": self.product}
        if path == "/Users":
            return 200, [self.user(key) for key in self.users.values()]
        if parts[0] == "Users" and len(parts) == 2 and parts[1] in self.users.values():
            return 200, self.user(parts[1])
        if path == "/ScheduledTasks":
            return 200, [{"Key": "RefreshLibrary", "State": "Idle",
                          "LastExecutionResult": {"Status": "Completed", "EndTimeUtc": str(self.refreshes)}}]
        if path == "/Items" and "ParentId" in query or parts[0] == "Playlists" and len(parts) == 3:
            found = self.collection(query["ParentId"][0] if "ParentId" in query else parts[1])
            if not found:
                return 404, None
            members = [{**self.movie(movie), "PlaylistItemId": entry} for entry, movie in found[3]]
            return 200, {"Items": members, "TotalRecordCount": len(members)}
        if path == "/Items" and query.get("IncludeItemTypes") == ["Movie"]:
            return 200, {"Items": [self.movie(m) for m in MOVIES]}
        if path == "/Items" and query.get("IncludeItemTypes") == ["Playlist,BoxSet"]:
            return 200, {"Items": [{"Id": key, "Type": kind, "Name": name}
                                   for kind, name, key, _ in sorted(self.collections, key=lambda c: c[1])]}
        if path == "/Items" and "Filters" in query:
            return 200, {"Items": self.filtered(query["Filters"][0], query.get("userId", [""])[0])}
        if parts[0] == "Items" and len(parts) == 2:
            if parts[1] in self.items:
                return 200, {**self.movie(parts[1]), **self.items[parts[1]]}
            found = self.collection(parts[1])
            return (200, {"Id": found[2], "Type": found[0], "Name": found[1]}) if found else (404, None)
        if parts[0] == "Items" and len(parts) == 3 and parts[2] == "Images":
            return 200, [{"ImageType": kind} for kind in self.images.get(parts[1], [])]
        if path == "/DisplayPreferences/usersettings":
            return 200, self.prefs.get(query["userId"][0], {"SortBy": "SortName", "SortOrder": "Ascending",
                                                           "CustomPrefs": {}})
        if path == "/Auth/Keys":
            return 200, {"Items": [{"AppName": app} for app in self.keys]}
        if path == "/Devices/Info":
            device = query["id"][0]
            return (200, {"CustomName": self.devices[device]}) if device in self.devices else (404, None)
        if path == "/Library/MediaFolders":
            return 200, {"Items": [{"Name": name} for name, _, _ in self.libraries]}
        if path == "/Library/VirtualFolders":
            return 200, [{"Name": name, "CollectionType": kind, "ItemId": key} for name, kind, key in self.libraries]
        return 200, {}

    def filtered(self, name, user):
        if user != ADMIN:
            plays = self.plays.get(user, [])
            return [{"Id": m, "UserData": {"Played": True, "PlayCount": 1}} for m in plays if name == "IsPlayed"]
        wanted = {"IsPlayed": lambda d: d["Played"], "IsFavorite": lambda d: d["IsFavorite"],
                  "IsResumable": lambda d: d["PlaybackPositionTicks"], "Likes": lambda d: d["Likes"] is True,
                  "Dislikes": lambda d: d["Likes"] is False}[name]
        return [self.movie(m) for m in MOVIES if wanted(self.data[m])]

    def post(self, path, query, body, auth):
        parts = path.strip("/").split("/")
        if parts[0] == "UserPlayedItems":
            self.data[parts[1]]["Played"] = True
        elif parts[0] == "UserFavoriteItems":
            self.data[parts[1]]["IsFavorite"] = True
        elif parts[0] == "UserItems" and parts[2] == "Rating":
            self.data[parts[1]]["Likes"] = query["likes"] == ["true"]
        elif parts[0] == "UserItems":
            self.data[parts[1]]["PlaybackPositionTicks"] = body["PlaybackPositionTicks"]
        elif path == "/Playlists":
            return 200, {"Id": self.create("Playlist", body["Name"], body["Ids"])}
        elif parts[0] == "Playlists" and len(parts) == 2:
            self.collection(parts[1])[1] = body["Name"]
        elif parts[0] == "Playlists" and len(parts) == 3:
            self.add(parts[1], query["ids"][0])
            return 204, None
        elif parts[0] == "Playlists" and parts[4] == "Move":
            entries = self.collection(parts[1])[3]
            entry = next(e for e in entries if e[0] == parts[3])
            entries.remove(entry)
            entries.insert(int(parts[5]), entry)
            return 204, None
        elif path == "/Collections":
            return 200, {"Id": self.create("BoxSet", query["name"][0], query["ids"][0].split(","))}
        elif parts[0] == "Collections":
            self.add(parts[1], query["ids"][0])
            return 204, None
        elif path == "/Users/New":
            key = f"{len(self.users):032x}"
            self.users[body["Name"]] = key
            return 200, {"Id": key}
        elif parts[0] == "Users" and parts[-1] == "Policy":
            self.accounts.setdefault(parts[1], {})["Policy"] = body
        elif path == "/Users/Configuration":
            self.accounts.setdefault(query["userId"][0], {})["Configuration"] = body
        elif path == "/DisplayPreferences/usersettings":
            self.prefs[query["userId"][0]] = body
        elif path == "/Auth/Keys":
            self.keys.append(query["app"][0])
            return 204, None
        elif path == "/Devices/Options":
            self.devices[query["id"][0]] = body["CustomName"]
            return 204, None
        elif path == "/Library/VirtualFolders":
            self.libraries.append([query["name"][0], query["collectionType"][0], f"{len(self.libraries) + 50:032x}"])
            return 204, None
        elif path == "/Sessions/Playing/Stopped":
            user = re.search(r'Token="token-(\w+)"', auth).group(1)
            self.plays.setdefault(user, []).append(body["ItemId"])
            return 204, None
        elif parts[0] == "Items" and len(parts) == 2:
            self.items[parts[1]].update({key: body[key] for key in ("Name", "LockData", "LockedFields", "Overview")})
            return 204, None
        elif parts[0] == "Items" and parts[2] == "Images":
            try:   # Jellyfin takes the image base64-encoded
                image = base64.b64decode(body or "", validate=True)
            except binascii.Error:
                image = b""
            if not image.startswith(b"\x89PNG"):
                return 400, None
            self.images[parts[1]] = sorted({*self.images.get(parts[1], []), parts[3]})
            return 204, None
        elif path == "/Library/Refresh":
            self.refreshes += 1
            return 204, None
        elif path == "/Users/AuthenticateByName":
            key = self.users.get(body["Username"])
            ok = key and answers.GATE_USERS.get(body["Username"]) == body["Pw"]
            return (200, {"AccessToken": f"token-{key}"}) if ok else (401, None)
        return 200, {}

    def delete(self, path, query, body, auth):
        parts = path.strip("/").split("/")
        if parts[0] == "Playlists":
            entries = self.collection(parts[1])[3]
            entries[:] = [e for e in entries if e[0] not in query["entryIds"][0].split(",")]
        elif parts[0] == "Collections":
            entries = self.collection(parts[1])[3]
            entries[:] = [e for e in entries if e[1] not in query["ids"][0].split(",")]
        elif path == "/Library/VirtualFolders":
            self.libraries = [lib for lib in self.libraries if lib[0] != query["name"][0]]
        elif parts[0] == "Items":
            self.collections = [c for c in self.collections if c[2] != parts[1]]
        return 204, None

    def create(self, kind, name, movies):
        key = f"{len(self.collections) + 100:032x}"
        self.collections.append([kind, name, key, []])
        for movie in movies:
            self.add(key, movie)
        return key

    def add(self, key, movie):
        entries = self.collection(key)[3]
        entries.append([f"{key[-3:]}-{len(entries)}-{movie[-2:]}", movie])

    def movie(self, key):
        return {"Id": key, "Name": self.items[key]["Name"], "RunTimeTicks": 600_000_000, "UserData": dict(self.data[key])}

    def save(self):
        if self.state:
            self.state.write_text(json.dumps({field: getattr(self, field) for field in FIELDS}))


def server(stub):
    """A ThreadingHTTPServer on a free loopback port answering through `stub`, not yet serving."""
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def handle_one(self, method):
            url = urlsplit(self.path)
            length = int(self.headers.get("Content-Length") or 0)
            raw = self.rfile.read(length) if length else b""
            json_body = (self.headers.get("Content-Type") or "").startswith("application/json")
            body = (json.loads(raw) if json_body else raw.decode()) if raw else None
            status, result = stub.answer(method, url.path, parse_qs(url.query), body,
                                         auth=self.headers.get("Authorization") or "")
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

        def do_DELETE(self):
            self.handle_one("DELETE")

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
