#!/usr/bin/env python3
"""The round trip's usage stage: what a user does in Ferrofin before going back, then Ferrofin's answers.

    usage.py --base-url URL --data-dir DIR --oracle-out FILE

The closing scan may take ROUNDTRIP_SCAN_TIMEOUT seconds (default 600), from the environment run.sh exports,
so the command line stays the contract's.

DIR is the copy the server runs on (it holds data/jellyfin.db); the stage acts through the copy's newest
API key, for the account smoke.sh probes as (answers.smoke_user). The minimal stage:
  1. three movies not yet played are marked played (the first three by sort name);
  2. two movies that are not favorites are favorited;
  3. a movie neither played nor started gets a resume position, half its runtime;
  4. a playlist of two movies and a collection of two movies are created;
  5. each account in answers.GATE_USERS is created with its password.
Then the library is refreshed and, once the scan has completed, the answers (answers.py) are written to
FILE: the oracle Jellyfin's answers are compared with after the return. Each action must show in them (a
write the server accepted and lost fails here, not as two equal answers later). Exit status 0; 1 with one
line on stderr naming the action that failed. The contract (README.md) stays when the actions change.
"""

import argparse
import os
import sqlite3
import sys
import urllib.parse

import answers
import api

PLAYLIST = "Round trip playlist"
COLLECTION = "Round trip collection"


class UsageError(Exception):
    """An action that failed, named."""


def act(server, action, method, path, body=None):
    status, result = server.send(method, path, body)
    if not 200 <= status < 300:
        raise UsageError(f"{action}: HTTP {status}")
    return result


def movies(server, user_id):
    query = urllib.parse.urlencode({"userId": user_id, "Recursive": "true", "IncludeItemTypes": "Movie",
                                    "SortBy": "SortName", "SortOrder": "Ascending"})
    found = (act(server, "list the movies", "GET", "/Items?" + query) or {}).get("Items") or []
    if len(found) < 6:
        raise UsageError(f"list the movies: {len(found)} found, the stage needs 6")
    return found


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


def run(base_url, data_dir, oracle_out, timeout):
    server = api.Server(base_url, data_dir)
    name, user = answers.smoke_user(data_dir)
    every = movies(server, user)
    data = {m["Id"]: m.get("UserData") or {} for m in every}
    played = [m for m in every if not data[m["Id"]].get("Played")][:3]
    for movie in played:
        act(server, f"mark {movie['Name']} played", "POST", f"/UserPlayedItems/{movie['Id']}?userId={user}")
    favorites = [m for m in every if not data[m["Id"]].get("IsFavorite")][:2]
    for movie in favorites:
        act(server, f"favorite {movie['Name']}", "POST", f"/UserFavoriteItems/{movie['Id']}?userId={user}")
    unstarted = [m for m in every if m not in played and not data[m["Id"]].get("Played")
                 and not data[m["Id"]].get("PlaybackPositionTicks") and m.get("RunTimeTicks")]
    if not unstarted:
        raise UsageError("set a resume position: no movie is unplayed, unstarted and has a runtime")
    resume, ticks = unstarted[0], unstarted[0]["RunTimeTicks"] // 2
    act(server, f"set a resume position on {resume['Name']}", "POST", f"/UserItems/{resume['Id']}/UserData?userId={user}",
        {"PlaybackPositionTicks": ticks})
    act(server, "create a playlist", "POST", "/Playlists",
        {"Name": PLAYLIST, "Ids": [m["Id"] for m in every[:2]], "UserId": user, "MediaType": "Video"})
    act(server, "create a collection", "POST", "/Collections?" + urllib.parse.urlencode(
        {"name": COLLECTION, "ids": ",".join(m["Id"] for m in every[2:4])}))
    for account, password in sorted(answers.GATE_USERS.items()):
        act(server, f"create the user {account}", "POST", "/Users/New", {"Name": account, "Password": password})
    try:
        server.scan(timeout)
    except api.CheckError as error:
        raise UsageError(f"refresh the library: {error}") from None
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
    for wanted, action in ((f"collection Playlist 2 {PLAYLIST}", "create a playlist"),
                           (f"collection BoxSet 2 {COLLECTION}", "create a collection"),
                           *((f"login {n} 200 token=yes", f"create the user {n}") for n in sorted(answers.GATE_USERS))):
        shown(action, wanted, wanted in lines)
    answers.write(oracle_out, lines)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--data-dir", required=True)
    parser.add_argument("--oracle-out", required=True)
    args = parser.parse_args(argv)
    try:
        timeout = float(os.environ.get("ROUNDTRIP_SCAN_TIMEOUT") or 600)
        run(args.base_url, args.data_dir, args.oracle_out, timeout)
    except (UsageError, api.CheckError) as error:
        print(f"usage: {error}", file=sys.stderr)
        return 1
    except (OSError, ValueError, KeyError, sqlite3.Error) as error:
        print(f"usage: {type(error).__name__}: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
