#!/usr/bin/env python3
"""The answers a round trip compares: what a server says about one copy, one line per answer.

usage.py writes them from Ferrofin before the stop (the stage's oracle), and run.sh writes them again
from Jellyfin after the return; through adoption_normalise (gates/adoption/lib.sh) the two files must
be equal. One capture serves both sides, so a probe added here is compared at once.

    answers.py --base-url URL --data-dir DIR --out FILE

The lines (README.md, "The usage-stage contract"):
    <status>  <metric>  <path>                                    gates/adoption/smoke.sh's probes, as it prints them
    userdata <user> <item> played=<0|1> fav=<0|1> resume=<ticks>  every item the IsPlayed, IsFavorite and
                                                                   IsResumable filters return, for every user, sorted
    login <user> <status> token=<yes|no>                          each account the usage stage creates (GATE_USERS),
                                                                   logging in with its password
    collection <Playlist|BoxSet> <items> <name>                   the smoke user's playlists and collections, and
                                                                   how many items each holds
The smoke probes run as ADOPTION_USER when it is set (run.sh sets the fixture's oracle user), as smoke.sh
does. Exit status 0 when the file was written; 1 with one line on stdout when the server could not be
asked (an answer the server refused is a line, never a failure: the comparison judges it).
"""

import argparse
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import urllib.parse

import api

HERE = Path(__file__).resolve().parent
ADOPTION = HERE.parent / "adoption"
#: The accounts the usage stage creates, with their passwords: synthetic test accounts, never a real
#: credential. Jellyfin must let each log in with the password Ferrofin stored.
GATE_USERS = {"roundtrip-user": "roundtrip-password-1"}
#: What a login sends besides the password: not "Jellyfin Web", so smoke.sh never picks the session it makes.
LOGIN_AUTH = 'MediaBrowser Client="roundtrip", Device="roundtrip", DeviceId="roundtrip-{name}", Version="1"'
USER_DATA_FILTERS = ("IsPlayed", "IsFavorite", "IsResumable")


def smoke_user(data_dir):
    """(name, id) of the account smoke.sh probes as: ADOPTION_USER, else its default choice
    (adoption_smoke_credentials in gates/adoption/lib.sh, asked rather than copied)."""
    result = subprocess.run(
        ["bash", "-c", '. "$1"; adoption_smoke_credentials "$2" "$3"', "-", str(ADOPTION / "lib.sh"),
         str(api.database(data_dir)), os.environ.get("ADOPTION_USER", "")],
        capture_output=True, text=True, check=False)
    fields = result.stdout.split()
    if result.returncode != 0 or len(fields) != 4:
        raise api.CheckError(f"no account to probe as: {result.stderr.strip() or 'no answer'}")
    return fields[0], fields[3]


def smoke_lines(base_url, data_dir):
    result = subprocess.run([str(ADOPTION / "smoke.sh"), base_url, str(data_dir)], capture_output=True,
                            text=True, check=False)
    if result.returncode != 0:
        raise api.CheckError(f"smoke.sh failed: {result.stderr.strip() or result.returncode}")
    return result.stdout.splitlines()


def items(server, query):
    """(status, the Items of one /Items-shaped answer)."""
    status, body = server.send("GET", query)
    return status, (body.get("Items") or []) if isinstance(body, dict) else []


def user_data_lines(server):
    status, users = server.send("GET", "/Users")
    if status != 200 or not isinstance(users, list):
        return [f"userdata /Users {status}"]
    out = []
    for user in sorted(users, key=lambda u: u.get("Name", "")):
        name, seen = user.get("Name"), {}
        for filters in USER_DATA_FILTERS:
            query = urllib.parse.urlencode({"userId": user.get("Id"), "Recursive": "true", "Filters": filters})
            status, found = items(server, "/Items?" + query)
            if status != 200:
                out.append(f"userdata {name} {filters} {status}")
            for item in found:
                seen[item.get("Id")] = item.get("UserData") or {}
        for key, data in sorted(seen.items()):
            out.append(f"userdata {name} {key} played={int(bool(data.get('Played')))} "
                       f"fav={int(bool(data.get('IsFavorite')))} resume={data.get('PlaybackPositionTicks') or 0}")
    return out


def login_lines(server):
    status, users = server.send("GET", "/Users")
    present = {u.get("Name") for u in users} if status == 200 and isinstance(users, list) else set()
    out = []
    for name, password in sorted(GATE_USERS.items()):
        if name not in present:
            continue
        status, body = server.send("POST", "/Users/AuthenticateByName", {"Username": name, "Pw": password},
                                   auth=LOGIN_AUTH.format(name=name))
        token = "yes" if isinstance(body, dict) and body.get("AccessToken") else "no"
        out.append(f"login {name} {status} token={token}")
    return out


def collection_lines(server, user_id):
    query = urllib.parse.urlencode({"userId": user_id, "Recursive": "true", "IncludeItemTypes": "Playlist,BoxSet",
                                    "SortBy": "SortName", "SortOrder": "Ascending"})
    status, found = items(server, "/Items?" + query)
    if status != 200:
        return [f"collection /Items {status}"]
    out = []
    for item in found:
        if item.get("Type") == "Playlist":
            path = f"/Playlists/{item.get('Id')}/Items?" + urllib.parse.urlencode({"userId": user_id})
        else:
            path = "/Items?" + urllib.parse.urlencode({"userId": user_id, "ParentId": item.get("Id")})
        status, body = server.send("GET", path)
        count = body.get("TotalRecordCount") if status == 200 and isinstance(body, dict) else f"error={status}"
        out.append(f"collection {item.get('Type')} {count} {item.get('Name')}")
    return out


def capture(base_url, data_dir):
    """Every answer, in file order."""
    server = api.Server(base_url, data_dir)
    _, user_id = smoke_user(data_dir)
    return (smoke_lines(base_url, data_dir) + user_data_lines(server) + login_lines(server)
            + collection_lines(server, user_id))


def write(path, lines):
    partial = Path(f"{path}.partial")
    partial.write_text("".join(f"{line}\n" for line in lines), encoding="utf-8")
    partial.replace(path)   # a file that exists is a whole one


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--data-dir", required=True)
    parser.add_argument("--out", required=True)
    args = parser.parse_args(argv)
    try:
        write(args.out, capture(args.base_url, args.data_dir))
    except api.CheckError as error:
        print(error)
        return 1
    except (OSError, ValueError, sqlite3.Error) as error:
        print(f"cannot read the copy or write {args.out}: {type(error).__name__}: {error}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
