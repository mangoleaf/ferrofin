#!/usr/bin/env python3
"""The round trip's usage stage: what a user does in Ferrofin before going back, then Ferrofin's answers.

    usage.py --base-url URL --data-dir DIR --oracle-out FILE

Each library scan may take ROUNDTRIP_SCAN_TIMEOUT seconds (default 600), from the environment run.sh exports,
so the command line stays the contract's.

DIR is the copy the server runs on (it holds data/jellyfin.db); the stage acts through the copy's newest
API key, for the account smoke.sh probes as (answers.smoke_user), and changes every Jellyfin-owned table a
user changes:
  1. watch state: three movies not yet played are marked played (the first three by sort name), two that are
     not favorites are favorited, one neither played nor started gets a resume position (half its runtime),
     and the last one is liked;
  2. accounts: each in answers.GATE_USERS is created, one with a password and one without. The first signs
     in, which makes a device, and plays a movie to its end; that device is then named. The second gets a
     narrower policy (one library, two sessions, hidden, no downloads) and other settings. An API key is made;
  3. the probed account's home-screen display preferences change;
  4. a playlist is created (two movies, then one added, one removed and the last moved first) and a collection
     (two movies, then one added and one removed), with a second of each to delete later;
  5. a library is added, and the copy's home-videos library removed.
The library is then scanned and the server left to settle: the metadata refresh each new playlist and
collection queued is no scheduled task, and one that saves an item after a change to it would undo the
change (or bring a deleted item back). Then
  6. the playlist is renamed, a movie's metadata edited and locked, and the collection given a primary image;
  7. the second playlist and collection are deleted. The copy's media is read-only, so the items the stage
     deletes are these, whose files live in the data directory.
Then the library is scanned and left to settle again, and the answers (answers.py) are written to FILE: the
oracle Jellyfin's answers are compared with after the return. Each action must show in them (a write the
server accepted and lost fails here, not as two equal answers later), and a deleted item must answer 404.
Each action's request and HTTP status go to FILE.actions.
Exit status 0; 1 with one line on stderr naming the action that failed. The contract (README.md) stays when
the actions change.
"""

import argparse
import base64
import os
import signal
import sqlite3
import struct
import sys
import urllib.parse
import zlib

import answers
import api

PLAYLIST, RENAMED, COLLECTION = "Round trip playlist", "Round trip playlist renamed", "Round trip collection"
DOOMED_PLAYLIST, DOOMED_COLLECTION = "Round trip deleted playlist", "Round trip deleted collection"
PLAYER, RESTRICTED = "roundtrip-user", "roundtrip-passwordless"   # both in answers.GATE_USERS
DEVICE_NAME, API_KEY_APP, ADDED_LIBRARY = "Round trip device", "Round trip", "Round trip library"
REMOVED_KIND, EDITED, OVERVIEW = "homevideos", "Round trip edited movie", "Edited by the round-trip usage stage."
#: Every request the stage made, "<action>  <HTTP status>", in order: FILE.actions.
RECORD = []


class UsageError(Exception):
    """An action that failed, named."""


def act(server, action, method, path, body=None, **send):
    try:
        status, result = server.send(method, path, body, **send)
    except api.CheckError as error:   # no answer at all: the server stopped or hung
        RECORD.append(f"{action}  no answer")
        raise UsageError(f"{action}: {error}") from None
    RECORD.append(f"{action}  {status}")
    if not 200 <= status < 300:
        raise UsageError(f"{action}: HTTP {status}")
    return result


def created(server, action, method, path, body=None, **send):
    """The Id of what one action created."""
    key = (act(server, action, method, path, body, **send) or {}).get("Id")
    if not key:
        raise UsageError(f"{action}: no Id in the answer")
    return key


def query(**params):
    return "?" + urllib.parse.urlencode(params)


def png(width, height, rgb):
    """A one-colour PNG, from the standard library alone."""
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
    rows = b"".join(b"\0" + bytes(rgb) * width for _ in range(height))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b""))


def movies(server, user_id):
    found = (act(server, "list the movies", "GET", "/Items" + query(
        userId=user_id, Recursive="true", IncludeItemTypes="Movie", SortBy="SortName", SortOrder="Ascending"))
        or {}).get("Items") or []
    if len(found) < 6:
        raise UsageError(f"list the movies: {len(found)} found, the stage needs 6")
    return found


def refresh(server, timeout, action):
    """A completed library scan, then no scheduled task running on two polls in a row. The settle is also
    the time the metadata refreshes the stage's changes queued get to finish: no task shows those."""
    try:
        server.scan(timeout)
        server.settle(timeout)
    except api.CheckError as error:
        RECORD.append(f"{action}  failed")
        raise UsageError(f"{action}: {error}") from None
    RECORD.append(f"{action}  completed")


def members(lines, kind, title):
    """The item names of one `members` line, or None."""
    names = after_the_prefix(lines, f"members {kind} {title} = ")
    return None if names is None else names.split(",")


def user_data(lines, name):
    """{item id: {"played": "1", "fav": "0", "resume": "0"}} for one account's userdata lines."""
    out = {}
    for fields in (line.split() for line in lines):
        if len(fields) == 6 and fields[0] == "userdata" and fields[1] == name:
            out[fields[2]] = dict(field.split("=", 1) for field in fields[3:])
    return out


def shown(action, wanted, found):
    if not found:
        raise UsageError(f"{action}: not in Ferrofin's answers (wanted {wanted})")


def after_the_prefix(lines, prefix):
    """The rest of the first line starting with prefix, or None."""
    return next((line[len(prefix):] for line in lines if line.startswith(prefix)), None)


def run(base_url, data_dir, oracle_out, timeout):
    server = api.Server(base_url, data_dir)
    name, user = answers.smoke_user(data_dir)
    every = movies(server, user)
    data = {m["Id"]: m.get("UserData") or {} for m in every}

    # 1. watch state
    played = [m for m in every if not data[m["Id"]].get("Played")][:3]
    for movie in played:
        act(server, f"mark {movie['Name']} played", "POST", f"/UserPlayedItems/{movie['Id']}" + query(userId=user))
    favorites = [m for m in every if not data[m["Id"]].get("IsFavorite")][:2]
    for movie in favorites:
        act(server, f"favorite {movie['Name']}", "POST", f"/UserFavoriteItems/{movie['Id']}" + query(userId=user))
    unstarted = [m for m in every if m not in played and not data[m["Id"]].get("Played")
                 and not data[m["Id"]].get("PlaybackPositionTicks") and m.get("RunTimeTicks")]
    if not unstarted:
        raise UsageError("set a resume position: no movie is unplayed, unstarted and has a runtime")
    resume, ticks = unstarted[0], unstarted[0]["RunTimeTicks"] // 2
    act(server, f"set a resume position on {resume['Name']}", "POST", f"/UserItems/{resume['Id']}/UserData"
        + query(userId=user), {"PlaybackPositionTicks": ticks})
    liked = every[-1]
    act(server, f"like {liked['Name']}", "POST", f"/UserItems/{liked['Id']}/Rating" + query(userId=user, likes="true"))

    # 2. accounts, a device, a play, an API key
    ids = {account: created(server, f"create the user {account}", "POST", "/Users/New",
                            {"Name": account, "Password": password})
           for account, password in sorted(answers.GATE_USERS.items())}
    signin = answers.LOGIN_AUTH.format(name=PLAYER)
    token = (act(server, f"sign {PLAYER} in", "POST", "/Users/AuthenticateByName",
                 {"Username": PLAYER, "Pw": answers.GATE_USERS[PLAYER]}, auth=signin) or {}).get("AccessToken")
    if not token:
        raise UsageError(f"sign {PLAYER} in: no AccessToken in the answer")
    watched = next((m for m in every if m.get("RunTimeTicks")), None)
    if not watched:
        raise UsageError("play a movie: no movie has a runtime")
    report = {"ItemId": watched["Id"], "PlaySessionId": "roundtrip", "PositionTicks": 0}
    player = f'{signin}, Token="{token}"'
    act(server, f"start playing {watched['Name']}", "POST", "/Sessions/Playing", report, auth=player)
    act(server, f"play {watched['Name']} to its end", "POST", "/Sessions/Playing/Stopped",
        {**report, "PositionTicks": watched["RunTimeTicks"]}, auth=player)
    device = answers.DEVICE_ID.format(name=PLAYER)
    act(server, "name the device", "POST", "/Devices/Options" + query(id=device), {"CustomName": DEVICE_NAME})
    act(server, "create an API key", "POST", "/Auth/Keys" + query(app=API_KEY_APP))
    account = act(server, f"read {RESTRICTED}", "GET", f"/Users/{ids[RESTRICTED]}") or {}
    libraries = sorted(act(server, "list the libraries", "GET", "/Library/VirtualFolders") or [],
                       key=lambda f: f.get("Name") or "")
    kept = [f for f in libraries if f.get("CollectionType") == "movies"]
    removed = [f["Name"] for f in libraries if f.get("CollectionType") == REMOVED_KIND]
    if not kept or not removed:
        raise UsageError(f"list the libraries: the copy needs a movies and a {REMOVED_KIND} library")
    act(server, f"narrow {RESTRICTED}'s policy", "POST", f"/Users/{ids[RESTRICTED]}/Policy", {
        **(account.get("Policy") or {}), "EnableAllFolders": False, "EnabledFolders": [kept[0]["ItemId"]],
        "EnableContentDownloading": False, "MaxActiveSessions": 2, "IsHidden": True})
    act(server, f"change {RESTRICTED}'s settings", "POST", "/Users/Configuration" + query(userId=ids[RESTRICTED]), {
        **(account.get("Configuration") or {}), "AudioLanguagePreference": "deu", "SubtitleMode": "Always",
        "EnableNextEpisodeAutoPlay": False})

    # 3. display preferences
    prefs_query = query(userId=user, client="emby")
    prefs = act(server, "read the display preferences", "GET", "/DisplayPreferences/usersettings" + prefs_query) or {}
    act(server, "change the display preferences", "POST", "/DisplayPreferences/usersettings" + prefs_query, {
        **prefs, "SortBy": "DateCreated", "SortOrder": "Descending",
        "CustomPrefs": {**(prefs.get("CustomPrefs") or {}), "homesection0": "latestmedia", "roundtrip": "on"}})

    # 4. a playlist and a collection, and one of each to delete. A playlist is its owner's to edit: upstream
    # answers an API key, which has no user, 400 on a move or a rename (GetUserById(Guid.Empty)).
    owner = answers.smoke_auth(data_dir)

    def playlist_of(title, items):
        return created(server, f"create the playlist {title}", "POST", "/Playlists", {
            "Name": title, "Ids": [m["Id"] for m in items], "UserId": user, "MediaType": "Video"}, auth=owner)

    def collection_of(title, items):
        return created(server, f"create the collection {title}", "POST", "/Collections" + query(
            name=title, ids=",".join(m["Id"] for m in items)))

    playlist, doomed_playlist = playlist_of(PLAYLIST, every[:2]), playlist_of(DOOMED_PLAYLIST, every[:1])
    act(server, "add to the playlist", "POST", f"/Playlists/{playlist}/Items" + query(ids=every[2]["Id"], userId=user),
        auth=owner)
    entries = (act(server, "list the playlist", "GET", f"/Playlists/{playlist}/Items" + query(userId=user), auth=owner)
               or {}).get("Items") or []
    first = next((e.get("PlaylistItemId") for e in entries if e.get("Id") == every[0]["Id"]), None)
    if first is None or len(entries) != 3:
        raise UsageError(f"add to the playlist: it lists {len(entries)} entries, wanted 3 with {every[0]['Name']}")
    act(server, "remove from the playlist", "DELETE", f"/Playlists/{playlist}/Items" + query(entryIds=first), auth=owner)
    act(server, "move in the playlist", "POST", f"/Playlists/{playlist}/Items/{entries[-1]['PlaylistItemId']}/Move/0",
        auth=owner)
    collection, doomed_collection = collection_of(COLLECTION, every[2:4]), collection_of(DOOMED_COLLECTION, every[4:5])
    act(server, "add to the collection", "POST", f"/Collections/{collection}/Items" + query(ids=every[4]["Id"]))
    act(server, "remove from the collection", "DELETE", f"/Collections/{collection}/Items" + query(ids=every[2]["Id"]))

    # 5. libraries
    act(server, "add a library", "POST", "/Library/VirtualFolders" + query(
        name=ADDED_LIBRARY, collectionType="movies", refreshLibrary="false"),
        {"LibraryOptions": {"EnableRealtimeMonitor": False, "PathInfos": []}})
    act(server, f"remove the library {removed[0]}", "DELETE", "/Library/VirtualFolders" + query(
        name=removed[0], refreshLibrary="true"))
    refresh(server, timeout, "refresh the library")

    # 6. and 7., once the refreshes the changes above queued have run
    act(server, "rename the playlist", "POST", f"/Playlists/{playlist}", {"Name": RENAMED}, auth=owner)
    edited = every[5]
    item = act(server, f"read {edited['Name']}", "GET", f"/Items/{edited['Id']}" + query(userId=user)) or {}
    item.pop("Trickplay", None)   # served but not bindable on 10.11.8 (gates/adoption/synthetic_fixture.py seed)
    act(server, f"edit and lock {edited['Name']}", "POST", f"/Items/{edited['Id']}", {
        **item, "Name": EDITED, "Overview": OVERVIEW, "LockData": True, "LockedFields": ["Name", "Overview"]})
    act(server, "give the collection a primary image", "POST", f"/Items/{collection}/Images/Primary",
        base64.b64encode(png(120, 180, (255, 165, 0))), content_type="image/png")
    act(server, f"delete the playlist {DOOMED_PLAYLIST}", "DELETE", f"/Items/{doomed_playlist}")
    act(server, f"delete the collection {DOOMED_COLLECTION}", "DELETE", f"/Items/{doomed_collection}")
    refresh(server, timeout, "refresh the library again")

    lines = answers.capture(base_url, data_dir)
    after = user_data(lines, name)
    for movie in played:
        shown(f"mark {movie['Name']} played", f"userdata {name} {movie['Id']} with played=1",
              after.get(movie["Id"], {}).get("played") == "1")
    for movie in favorites:
        shown(f"favorite {movie['Name']}", f"userdata {name} {movie['Id']} with fav=1",
              after.get(movie["Id"], {}).get("fav") == "1")
    shown(f"set a resume position on {resume['Name']}", f"userdata {name} {resume['Id']} with resume={ticks}",
          after.get(resume["Id"], {}).get("resume") == str(ticks))
    restricted = after_the_prefix(lines, f"user {RESTRICTED} ") or ""
    libraries_after = (after_the_prefix(lines, "libraries ") or "").split(",")
    image = (after_the_prefix(lines, f"image BoxSet {COLLECTION} ") or "").split(" ")[0].split(",")
    in_playlist, in_collection = members(lines, "Playlist", RENAMED) or [], members(lines, "BoxSet", COLLECTION) or []
    gone = {doomed: server.send("GET", f"/Items/{doomed}" + query(userId=user))[0]
            for doomed in (doomed_playlist, doomed_collection)}
    for action, wanted, found in (
            (f"like {liked['Name']}", f"userplays {name} {liked['Id']} with likes=true",
             (after_the_prefix(lines, f"userplays {name} {liked['Id']} ") or "").endswith(" likes=true")),
            (f"play {watched['Name']} to its end", f"userdata {PLAYER} {watched['Id']} with played=1",
             user_data(lines, PLAYER).get(watched["Id"], {}).get("played") == "1"),
            ("name the device", f"device {device} name={DEVICE_NAME}", f"device {device} name={DEVICE_NAME}" in lines),
            ("create an API key", f"apikeys with {API_KEY_APP}",
             API_KEY_APP in (after_the_prefix(lines, "apikeys ") or "").split(",")),
            (f"narrow {RESTRICTED}'s policy", "hidden=1 folders=1 download=0 sessions=2",
             all(f in restricted for f in ("hidden=1 ", "folders=1 ", "download=0 ", "sessions=2 "))),
            (f"change {RESTRICTED}'s settings", "audio=deu subs=Always autoplay=0",
             "audio=deu subs=Always autoplay=0" in restricted),
            ("change the display preferences", f"prefs {name} DateCreated Descending with roundtrip=on",
             "roundtrip=on" in (after_the_prefix(lines, f"prefs {name} DateCreated Descending ") or "").split(",")),
            ("add a library", f"libraries with {ADDED_LIBRARY}", ADDED_LIBRARY in libraries_after),
            (f"remove the library {removed[0]}", f"libraries without {removed[0]}", removed[0] not in libraries_after),
            ("rename the playlist", f"a collection Playlist line for {RENAMED}",
             any(line.startswith("collection Playlist ") and line.endswith(f" {RENAMED}") for line in lines)),
            ("add to the playlist", f"{every[2]['Name']} in the playlist", every[2]["Name"] in in_playlist),
            ("remove from the playlist", f"{every[0]['Name']} not in the playlist",
             bool(in_playlist) and every[0]["Name"] not in in_playlist),
            ("move in the playlist", f"members Playlist {RENAMED} = {every[2]['Name']},{every[1]['Name']}",
             in_playlist == [every[2]["Name"], every[1]["Name"]]),
            ("add to the collection", f"{every[4]['Name']} in the collection", every[4]["Name"] in in_collection),
            ("remove from the collection", f"members BoxSet {COLLECTION} = {every[3]['Name']},{every[4]['Name']} by name",
             in_collection == sorted([every[3]["Name"], every[4]["Name"]])),
            ("give the collection a primary image", f"image BoxSet {COLLECTION} with Primary", "Primary" in image),
            (f"edit and lock {edited['Name']}", f"item {EDITED} lock=1 fields=Name,Overview overview={len(OVERVIEW)}",
             f"item {EDITED} lock=1 fields=Name,Overview overview={len(OVERVIEW)}" in lines),
            (f"delete the playlist {DOOMED_PLAYLIST}", "GET /Items/<its id> 404 and no line for it",
             gone[doomed_playlist] == 404 and not any(line.endswith(f" {DOOMED_PLAYLIST}") for line in lines)),
            (f"delete the collection {DOOMED_COLLECTION}", "GET /Items/<its id> 404 and no line for it",
             gone[doomed_collection] == 404 and not any(line.endswith(f" {DOOMED_COLLECTION}") for line in lines)),
            *((f"create the user {n}", f"login {n} 200 token=yes", f"login {n} 200 token=yes" in lines)
              for n in sorted(answers.GATE_USERS))):
        shown(action, wanted, found)
    answers.write(oracle_out, lines)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--data-dir", required=True)
    parser.add_argument("--oracle-out", required=True)
    args = parser.parse_args(argv)
    # The harness's deadline sends TERM: exit through the finally below, so FILE.actions says how far it got.
    previous = signal.signal(signal.SIGTERM, lambda *_: sys.exit(1))
    RECORD.clear()
    status = 1
    try:
        timeout = float(os.environ.get("ROUNDTRIP_SCAN_TIMEOUT") or 600)
        run(args.base_url, args.data_dir, args.oracle_out, timeout)
        status = 0
    except (UsageError, api.CheckError) as error:
        print(f"usage: {error}", file=sys.stderr)
    except (OSError, ValueError, KeyError, TypeError, AttributeError, IndexError, sqlite3.Error) as error:
        print(f"usage: {type(error).__name__}: {error}", file=sys.stderr)
    finally:
        signal.signal(signal.SIGTERM, previous)
        try:
            answers.write(f"{args.oracle_out}.actions", RECORD)
        except OSError as error:
            print(f"usage: cannot write {args.oracle_out}.actions: {error}", file=sys.stderr)
            status = 1
    return status


if __name__ == "__main__":
    sys.exit(main())
