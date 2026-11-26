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
                                                          answers once its start failed; "setup": for SETUP_SECONDS,
                                                          /System/Info/Public 200 and everything else 503, as that
                                                          server answers while Jellyfin's last migrations run, then
                                                          for as long /System/Ping 200 and everything else 503, as
                                                          the real server answers until its core startup completes,
                                                          then "ok"; "loses": the copy's first favorite is gone, a
                                                          return that lost a write; "reshares": a playlist's metadata
                                                          refresh shares it again with the account its stale
                                                          playlist.xml names, as Jellyfin's refresh merges the file;
                                                          "churns": a library scan deletes SHARED's row, as a scan that
                                                          removes an item would; "exits": the server stops (status 3)
                                                          as soon as its port is written. ROLE names the product.
    fake_server.py export OUT                             what the export does to the copy it wrote to OUT: drops the
                                                          tables of Ferrofin's from OUT/data/jellyfin.db and lists each
                                                          in OUT's loss report. FAKE_EXPORT=unreported leaves them out
                                                          of the report, =extra-table keeps them in the database, and
                                                          =drops-config loses the settings the copy's state holds, as
                                                          an export that wrote no configuration document would

The copy holds SHARED, a playlist of synthetic-adult's (answers.SHARE_OWNER) shared with synthetic-child, with a
playlist.xml listing the share, as the fixtures' Synthetic mixed order is, and PRIVATE, a playlist of synthetic-adult's
holding two movies, as the fixtures' Synthetic private playlist is. Besides seven movies it lists the fixtures' merged
versions (VERSIONS): two pairs, each shown as its primary with two media sources, and a third copy; a merge makes the
first id given the primary, and synthetic-child is limited by parental rating. synthetic-adult also sees the playlists the
stage makes, whose shares it is refused (403), as upstream refuses a playlist's shares to anyone but its owner. A full
refresh of SHARED (POST /Items/SHARED/Refresh?metadataRefreshMode=FullRefresh) moves its DateLastRefreshed in
CONFIG/data/jellyfin.db a moment later, as Jellyfin's queued refresh does; a refresh of another mode moves nothing.
Live TV answers as livetv.py asks it: a tuner and a guide, whose guide refresh (the RefreshGuide task) gives two
channels and their programmes, timers, series timers and favourite channels.
"""

import base64
import binascii
from contextlib import closing
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import sqlite3
import sys
import threading
import time
from urllib.parse import parse_qs, urlsplit
import uuid

sys.path.append(str(Path(__file__).resolve().parent.parent))
import answers  # noqa: E402

ADMIN = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
ADULT, CHILD = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "cccccccccccccccccccccccccccccccc"
#: synthetic-adult's playlist, shared with synthetic-child, and its name.
SHARED, SHARED_NAME = "dddddddddddddddddddddddddddddddd", "Synthetic mixed order"
MOVIES = [f"{n:032x}" for n in range(1, 8)]   # seven movies; the first one is already played
#: The fixtures' versions, {id: name}, listed between the sixth movie and the last (as their names sort), and which of
#: them the fixtures merged into which: {version: primary}.
VERSIONS = {f"{n:032x}": name for n, name in enumerate(
    ("Rowan Ferry", "Rowan Ferry Extended", "Saffron Gate", "Saffron Gate Extended", "Saffron Gate Remastered"), 0x20)}
MERGED = {f"{0x21:032x}": f"{0x20:032x}", f"{0x23:032x}": f"{0x22:032x}"}
LISTED = [*MOVIES[:6], *VERSIONS, MOVIES[6]]
#: synthetic-adult's playlist the fixtures filled, and its name.
PRIVATE, PRIVATE_NAME = "9" * 32, "Synthetic private playlist"
STATE = "fake-state.json"
#: How long a "setup" server answers as Jellyfin's setup server does, and then as its starting real server does:
#: each longer than the harness's poll interval.
SETUP_SECONDS = 3
#: How long after its request a queued refresh lands.
REFRESH_SECONDS = 0.2
#: What the stub keeps, and saves to CONFIG/fake-state.json.
FIELDS = ("refreshes", "users", "data", "collections", "accounts", "prefs", "keys", "devices", "libraries", "items",
          "images", "plays", "shares", "config", "options", "livetv", "versions", "private")
#: Jellyfin's schema, which the copy's database has (gates/roundtrip/lib.sh RT_SCHEMA).
SCHEMA = Path(__file__).resolve().parents[3] / "crates" / "ferrofin-db" / "tests" / "data" / "jellyfin-12.2-schema.sql"
#: The export's loss report (lib.sh RT_LOSS_REPORT).
LOSS_REPORT = "ferrofin-export-report.txt"
#: The id of the RefreshGuide task.
GUIDE_TASK = "6c5d7b3a2e1f4a0b9c8d7e6f5a4b3c2d"


def fresh():
    return {
        "refreshes": 0,
        "users": {"synthetic-admin": ADMIN, "synthetic-adult": ADULT, "synthetic-child": CHILD},
        "data": {m: {"Played": m == MOVIES[0], "IsFavorite": False, "PlaybackPositionTicks": 0, "PlayCount": 0,
                     "Likes": None} for m in LISTED},
        "collections": [],   # [type, name, id, [[entry id, movie id], ...]]
        "accounts": {},      # user id: {"Policy": {...}, "Configuration": {...}}
        "prefs": {},         # user id: display preferences
        "keys": ["Synthetic adoption"],
        "devices": {},       # device id: custom name
        "libraries": [["Synthetic Movies", "movies", "1" * 32], ["Synthetic Photos", "homevideos", "2" * 32]],
        "items": {m: {"Name": VERSIONS.get(m, f"Movie {m[-1]}"), "LockData": False, "LockedFields": [], "Overview": "x"}
                  for m in LISTED},
        "images": {},        # item id: image types
        "plays": {},         # user id (not ADMIN): movies played to the end
        "shares": [CHILD],   # whom SHARED is shared with
        "config": {},        # document ("system", "encoding", …): the settings posted to it
        "options": {},       # library name: the LibraryOptions it was added with
        "livetv": {"tuner": False, "guide": False, "refreshes": 0, "channels": [], "programmes": [], "timers": [],
                   "series": [], "favourites": []},
        "versions": dict(MERGED),   # version: its primary
        "private": [["p-0", MOVIES[0]], ["p-1", MOVIES[1]]],   # PRIVATE's entries
    }


class Stub:
    """fail: a path prefix answered 500. drop: a path prefix (or, ending in $, a whole path; after "POST " or
    "DELETE ", that method's only) answered 200 but not kept (a write that was lost)."""

    def __init__(self, fail="", drop="", state=None, product="Jellyfin Server", behaviour="ok", db=None):
        self.fail, self.drop, self.state, self.product, self.behaviour = fail, drop, state, product, behaviour
        self.db = db or (state.parent / "data" / "jellyfin.db" if state else None)
        self.calls, self.lock = [], threading.Lock()
        self.setup_until = time.monotonic() + SETUP_SECONDS
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
            now = time.monotonic()
            if self.behaviour == "setup" and now < self.setup_until + SETUP_SECONDS:
                # the setup server, then the real server before its core startup completed
                if path != ("/System/Info/Public" if now < self.setup_until else "/System/Ping"):
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
        limit = {"MaxParentalRating": 13} if key == CHILD else {}
        return {"Name": name, "Id": key,
                "Policy": {"IsAdministrator": key == ADMIN, "EnableAllFolders": True, **limit, **account.get("Policy", {})},
                "Configuration": {"SubtitleMode": "Default", **account.get("Configuration", {})}}

    def collection(self, key):
        if key in (PRIVATE, SHARED):   # SHARED's audio entries play no part here
            return ["Playlist", PRIVATE_NAME, PRIVATE, self.private] if key == PRIVATE else ["Playlist", SHARED_NAME, SHARED, []]
        return next((c for c in self.collections if c[2] == key), None)

    def guide_task(self):
        return {"Key": "RefreshGuide", "Id": GUIDE_TASK, "State": "Idle",
                "LastExecutionResult": {"Status": "Completed", "EndTimeUtc": str(self.livetv["refreshes"])}}

    def livetv_get(self, path, query):
        """The answer to a Live TV read, or None for another path."""
        tv = self.livetv
        if path == "/LiveTv/Channels":
            favourite = query.get("IsFavorite") == ["true"]
            channels = [c for c in tv["channels"] if not favourite or c["Id"] in tv["favourites"]]
            return 200, {"Items": channels, "TotalRecordCount": len(channels)}
        if path == "/LiveTv/Programs":
            return 200, {"Items": tv["programmes"], "TotalRecordCount": len(tv["programmes"])}
        if path == "/LiveTv/Timers":
            return 200, {"Items": tv["timers"], "TotalRecordCount": len(tv["timers"])}
        if path == "/LiveTv/SeriesTimers":
            return 200, {"Items": tv["series"], "TotalRecordCount": len(tv["series"])}
        if path == "/LiveTv/Timers/Defaults":
            return 200, {"ProgramId": query["programId"][0]}
        if path == f"/ScheduledTasks/{GUIDE_TASK}":
            return 200, self.guide_task()
        return None

    def livetv_post(self, path, body):
        """The answer to a Live TV write, or None for another path."""
        tv = self.livetv
        if path in ("/LiveTv/TunerHosts", "/LiveTv/ListingProviders"):
            tv["tuner" if path.endswith("TunerHosts") else "guide"] = True
            return 200, {"Id": path.rsplit("/", 1)[1]}
        if path == f"/ScheduledTasks/Running/{GUIDE_TASK}":
            tv["refreshes"] += 1
            if tv["tuner"]:
                tv["channels"] = [{"Id": "e" * 32, "Name": "Test News"}, {"Id": "f" * 32, "Name": "Test Films"}]
            if tv["tuner"] and tv["guide"]:
                tv["programmes"] = [{"Id": f"{n:032x}"[::-1], "Name": name}
                                    for n, name in enumerate(["Quiz Night", "The Long Road"], start=1)]
            return 204, None
        if path in ("/LiveTv/Timers", "/LiveTv/SeriesTimers"):
            timers = tv["timers" if path.endswith("/Timers") else "series"]
            timers.append({"Id": f"timer-{len(tv['timers']) + len(tv['series'])}", "ProgramId": body["ProgramId"]})
            return 204, None
        if path.startswith("/UserFavoriteItems/") and any(path.endswith(c["Id"]) for c in tv["channels"]):
            tv["favourites"].append(path.rsplit("/", 1)[1])
            return 200, {}
        return None

    def get(self, path, query):
        parts = path.strip("/").split("/")
        if (live := self.livetv_get(path, query)) is not None:
            return live
        if path == "/System/Info/Public":
            return 200, {"Version": "12.4.0", "ProductName": self.product}
        if path == "/System/Ping":
            return 200, self.product
        if path == "/Users":
            return 200, [self.user(key) for key in self.users.values()]
        if parts[0] == "Users" and len(parts) == 2 and parts[1] in self.users.values():
            return 200, self.user(parts[1])
        if path == "/ScheduledTasks":
            return 200, [{"Key": "RefreshLibrary", "State": "Idle",
                          "LastExecutionResult": {"Status": "Completed", "EndTimeUtc": str(self.refreshes)}},
                         self.guide_task()]
        if parts[0] == "Playlists" and len(parts) == 3 and parts[2] == "Users":
            if parts[1] != SHARED:
                return 403, None
            return 200, [{"UserId": user, "CanEdit": False} for user in self.shares]
        if path == "/Items" and query.get("IncludeItemTypes") == ["Playlist"] and query.get("userId") == [ADULT]:
            others = [{"Id": key, "Type": kind, "Name": name} for kind, name, key, _ in self.collections
                      if kind == "Playlist"]
            return 200, {"Items": [{"Id": SHARED, "Type": "Playlist", "Name": SHARED_NAME},
                                   {"Id": PRIVATE, "Type": "Playlist", "Name": PRIVATE_NAME}, *others]}
        if path == "/Items" and "ParentId" in query or parts[0] == "Playlists" and len(parts) == 3:
            found = self.collection(query["ParentId"][0] if "ParentId" in query else parts[1])
            if not found:
                return 404, None
            members = [{**self.movie(movie), "PlaylistItemId": entry} for entry, movie in found[3]]
            return 200, {"Items": members, "TotalRecordCount": len(members)}
        if path == "/Items" and query.get("IncludeItemTypes") == ["Movie"]:
            return 200, {"Items": [self.movie(m) for m in LISTED if m not in self.versions]}
        if parts[0] == "Videos" and parts[2:] == ["AdditionalParts"]:
            return 200, {"Items": [], "TotalRecordCount": 0}
        if path == "/Items" and query.get("IncludeItemTypes") == ["Playlist,BoxSet"]:
            return 200, {"Items": [{"Id": key, "Type": kind, "Name": name}
                                   for kind, name, key, _ in sorted(self.collections, key=lambda c: c[1])]}
        if path == "/Items" and "Filters" in query:
            return 200, {"Items": self.filtered(query["Filters"][0], query.get("userId", [""])[0])}
        if parts[0] == "Items" and len(parts) == 2:
            if parts[1] in self.items:
                sources = [parts[1], *(v for v, primary in self.versions.items() if primary == parts[1])]
                return 200, {**self.movie(parts[1]), **self.items[parts[1]], "MediaSources": [{"Id": s} for s in sources]}
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
            return 200, [{"Name": name, "CollectionType": kind, "ItemId": key, "LibraryOptions": self.options.get(name, {})}
                         for name, kind, key in self.libraries]
        if parts[:2] == ["System", "Configuration"]:
            return 200, self.config.get(parts[2] if len(parts) > 2 else "system", {})
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
        if (live := self.livetv_post(path, body)) is not None:
            return live
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
            self.options[query["name"][0]] = (body or {}).get("LibraryOptions") or {}
            return 204, None
        elif path == "/Sessions/Playing/Stopped":
            user = re.search(r'Token="token-(\w+)"', auth).group(1)
            self.plays.setdefault(user, []).append(body["ItemId"])
            return 204, None
        elif parts[0] == "Items" and len(parts) == 2:
            self.items[parts[1]].update({key: body[key] for key in ("Name", "LockData", "LockedFields", "Overview")})
            return 204, None
        elif parts[0] == "Items" and parts[2] == "Refresh":
            if query.get("metadataRefreshMode") == ["FullRefresh"]:
                if self.behaviour == "reshares":
                    self.shares = [CHILD]
                if self.db:   # queued: the stamp moves after the answer
                    threading.Timer(REFRESH_SECONDS, self.refreshed, (parts[1],)).start()
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
            if self.behaviour == "churns" and self.db:
                with closing(sqlite3.connect(self.db)) as db:
                    db.execute("PRAGMA foreign_keys = ON")   # its image row goes with it, as in Jellyfin's schema
                    db.execute("DELETE FROM BaseItems WHERE Id = ?", (str(uuid.UUID(SHARED)).upper(),))
                    db.commit()
            return 204, None
        elif parts[:2] == ["System", "Configuration"]:
            self.config[parts[2] if len(parts) > 2 else "system"] = body
            return 204, None
        elif path == "/Videos/MergeVersions":
            primary, *others = query["ids"][0].split(",")
            self.versions.update({other: primary for other in others})
            return 204, None
        elif path == "/Users/AuthenticateByName":
            key = self.users.get(body["Username"])
            ok = key and answers.GATE_USERS.get(body["Username"]) == body["Pw"]
            return (200, {"AccessToken": f"token-{key}"}) if ok else (401, None)
        return 200, {}

    def delete(self, path, query, body, auth):
        parts = path.strip("/").split("/")
        if parts[0] == "Playlists" and parts[2] == "Users":
            self.shares = [user for user in self.shares if user != parts[3]]
        elif parts[0] == "Playlists":
            entries = self.collection(parts[1])[3]
            entries[:] = [e for e in entries if e[0] not in query["entryIds"][0].split(",")]
        elif parts[0] == "Collections":
            entries = self.collection(parts[1])[3]
            entries[:] = [e for e in entries if e[1] not in query["ids"][0].split(",")]
        elif path == "/Library/VirtualFolders":
            self.libraries = [lib for lib in self.libraries if lib[0] != query["name"][0]]
        elif parts[0] == "Videos" and parts[2:] == ["AlternateSources"]:
            self.versions = {v: primary for v, primary in self.versions.items() if primary != parts[1]}
        elif parts[0] == "Items":
            self.collections = [c for c in self.collections if c[2] != parts[1]]
        return 204, None

    def refreshed(self, key):
        with closing(sqlite3.connect(self.db)) as db:
            db.execute("UPDATE BaseItems SET DateLastRefreshed = datetime('now', '+1 second') WHERE Id = ?",
                       (str(uuid.UUID(key)).upper(),))
            db.commit()

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


def insert(db, table, **values):
    """A row of `table` with `values`, every other column Jellyfin's schema needs a value for given 0 or '' (a
    numbered key SQLite's)."""
    for _, column, kind, needed, default, key in db.execute(f'PRAGMA table_info("{table}")'):
        if needed and default is None and not key and column not in values:
            values[column] = 0 if kind.upper() in ("INTEGER", "REAL") else ""
    db.execute(f'INSERT INTO "{table}" ({", ".join(values)}) VALUES ({", ".join("?" * len(values))})',
               tuple(values.values()))


def copy(root):
    """A data directory holding Jellyfin's schema and the rows the stages read: an API key, an administrator and
    synthetic-adult each with a web session, and the rows (with their folders) of SHARED, whose playlist.xml lists its
    share and which has a primary image, and of a library; and a table of Ferrofin's with a row, as an adopted copy
    has."""
    for folder in ("data/playlists/Synthetic mixed order", "root/default/Synthetic Movies", "metadata"):
        (root / folder).mkdir(parents=True)
    (root / "metadata/playlist.jpg").write_bytes(b"\xff\xd8\xff\xe0 a jpeg")
    (root / "data/playlists/Synthetic mixed order/playlist.xml").write_text(
        f"<Item><Shares><Share><UserId>{uuid.UUID(CHILD)}</UserId></Share></Shares></Item>", encoding="utf-8")
    admin, adult = str(uuid.UUID(ADMIN)).upper(), str(uuid.UUID(ADULT)).upper()
    with closing(sqlite3.connect(root / "data" / "jellyfin.db")) as db:
        db.execute("PRAGMA synchronous = OFF")   # a throwaway file: no flush per statement
        db.executescript(SCHEMA.read_text(encoding="utf-8"))
        insert(db, "ApiKeys", Id=1, Name="Synthetic adoption", AccessToken="apikey", DateCreated="2026-01-01",
               DateLastActivity="2026-01-01")
        for key, name, token, device in ((admin, "synthetic-admin", "usertoken", "device"),
                                         (adult, "synthetic-adult", "adulttoken", "adult")):
            insert(db, "Users", Id=key, Username=name, NormalizedUsername=name.upper())
            insert(db, "Devices", UserId=key, AccessToken=token, DeviceId=device, AppName="Jellyfin Web",
                   DateLastActivity="2026-01-02")
        insert(db, "Permissions", UserId=admin, Kind=0, Value=1)
        insert(db, "BaseItems", Id=str(uuid.UUID(SHARED)).upper(), Type="MediaBrowser.Controller.Playlists.Playlist",
               Name=SHARED_NAME, Path="%AppDataPath%/playlists/Synthetic mixed order", DateCreated="2026-01-01 00:00:00",
               DateLastRefreshed="2026-01-01 00:00:00", IsFolder=1)
        insert(db, "BaseItemImageInfos", Id="12121212-1212-1212-1212-121212121212", ItemId=str(uuid.UUID(SHARED)).upper(),
               Path="%MetadataPath%/playlist.jpg", ImageType=0, DateModified="2026-01-01 00:00:00", Width=1, Height=1)
        insert(db, "BaseItems", Id="11111111-1111-1111-1111-111111111111", Name="Synthetic Movies",
               Type="MediaBrowser.Controller.Entities.CollectionFolder", Path="/config/root/default/Synthetic Movies",
               DateCreated="2026-01-01 00:00:00", IsFolder=1)
        db.executescript("CREATE TABLE FerrofinAdoption (Generation TEXT); INSERT INTO FerrofinAdoption VALUES ('fake');")
        db.commit()
    return root


def export(out, knob=""):
    """What the export does to the copy it wrote to `out`: the tables of Ferrofin's dropped, each in the loss report
    with its count; `knob` (FAKE_EXPORT) breaks one part (the module's docstring)."""
    with closing(sqlite3.connect(out / "data" / "jellyfin.db")) as db:
        held = [(name, db.execute(f'SELECT count(*) FROM "{name}"').fetchone()[0]) for (name,) in db.execute(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE 'Ferrofin%'").fetchall()]
        if knob != "extra-table":
            for name, _ in held:
                db.execute(f'DROP TABLE "{name}"')
        db.commit()
    lines = [f"  {name} ({rows} {'row' if rows == 1 else 'rows'}): a table of Ferrofin's." for name, rows in held]
    (out / LOSS_REPORT).write_text("What the export to Jellyfin left behind\n\n"
                                   + "".join(f"{line}\n" for line in lines if knob != "unreported"), encoding="utf-8")
    state = out / STATE
    if knob == "drops-config" and state.exists():
        saved = json.loads(state.read_text())
        saved.pop("config", None)
        state.write_text(json.dumps(saved))


def main(argv):
    if len(argv) == 2 and argv[0] == "copy":
        copy(Path(argv[1]))
        return 0
    if len(argv) == 2 and argv[0] == "export":
        export(Path(argv[1]), os.environ.get("FAKE_EXPORT", ""))
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
