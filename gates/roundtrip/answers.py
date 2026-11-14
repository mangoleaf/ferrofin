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
    userplays <user> <item> count=<n> likes=<true|false|->        every item the IsPlayed and Likes filters return,
                                                                   for every user, sorted
    user <name> admin=<0|1> disabled=… folders=… sessions=…       every account's policy and settings, by name
    badlogin <user> <status>                                      each account in GATE_USERS with a password, logging
                                                                   in with a wrong one (before its right one, so that
                                                                   resets the failed-attempt count)
    login <user> <status> token=<yes|no>                          each account the usage stage creates (GATE_USERS),
                                                                   logging in with its password
    device <id> name=<custom name>                                the device each of those logins is
    prefs <user> <sort by> <sort order> <custom prefs>            every account's home-screen display preferences
    apikeys <app names>                                           the API keys, by app name
    libraries <names>                                             the libraries (/Library/MediaFolders), by name
    collection <Playlist|BoxSet> <items> <name>                   the smoke user's playlists and collections, and
                                                                   how many items each holds
    members <Playlist|BoxSet> <name> = <item names>               their items: a playlist's in its order, a
                                                                   collection's by name
    image <Playlist|BoxSet> <name> <image types>                  which images they have
    item <name> lock=<0|1> fields=<locked fields> overview=<n>    the smoke user's movies, by name
    config <document> <setting>=<value> …                         server settings a dashboard shows, per document
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
#: credential. Jellyfin must let each log in with the password Ferrofin stored ("": none).
GATE_USERS = {"roundtrip-user": "roundtrip-password-1", "roundtrip-passwordless": ""}
#: What a login sends besides the password: not "Jellyfin Web", so smoke.sh never picks the session it makes.
DEVICE_ID = "roundtrip-{name}"
LOGIN_AUTH = f'MediaBrowser Client="roundtrip", Device="roundtrip", DeviceId="{DEVICE_ID}", Version="1"'
USER_DATA_FILTERS = ("IsPlayed", "IsFavorite", "IsResumable")
# Not Dislikes: it is "not liked", a mixed listing of nearly everything, where upstream shows other users'
# private playlists and Ferrofin does not (an accepted divergence).
USER_PLAYS_FILTERS = ("IsPlayed", "Likes")
#: Scalar settings a dashboard shows, per configuration document (/System/Configuration/<key>).
CONFIGURATION = {
    "system": ("UICulture", "PreferredMetadataLanguage", "MetadataCountryCode", "MinResumePct", "MaxResumePct",
               "MinResumeDurationSeconds", "LibraryMonitorDelay", "EnableFolderView",
               "EnableGroupingMoviesIntoCollections", "EnableGroupingShowsIntoCollections",
               "DisplaySpecialsWithinSeasons", "QuickConnectAvailable", "ActivityLogRetentionDays"),
    "encoding": ("EncodingThreadCount", "HardwareAccelerationType", "EnableThrottling", "ThrottleDelaySeconds",
                 "H264Crf", "H265Crf", "EnableTonemapping", "DownMixAudioBoost"),
    "network": ("EnableRemoteAccess", "InternalHttpPort", "EnableHttps", "BaseUrl", "EnableIPv6"),
}


def smoke_credentials(data_dir):
    """(name, token, device id, id) of the account smoke.sh probes as: ADOPTION_USER, else its default choice
    (adoption_smoke_credentials in gates/adoption/lib.sh, asked rather than copied)."""
    result = subprocess.run(
        ["bash", "-c", '. "$1"; adoption_smoke_credentials "$2" "$3"', "-", str(ADOPTION / "lib.sh"),
         str(api.database(data_dir)), os.environ.get("ADOPTION_USER", "")],
        capture_output=True, text=True, check=False)
    fields = result.stdout.split()
    if result.returncode != 0 or len(fields) != 4:
        raise api.CheckError(f"no account to probe as: {result.stderr.strip() or 'no answer'}")
    return tuple(fields)


def smoke_user(data_dir):
    """(name, id) of the account smoke.sh probes as."""
    name, _, _, key = smoke_credentials(data_dir)
    return name, key


def smoke_auth(data_dir):
    """The Authorization header of that account's own web session, as smoke.sh sends it."""
    _, token, device, _ = smoke_credentials(data_dir)
    return f'MediaBrowser Token="{token}", Client="Jellyfin Web", Device="Chrome", DeviceId="{device}", Version="10.11.8"'


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


def flag(value):
    return int(bool(value))


def accounts(server):
    """(status, every account sorted by name)."""
    status, users = server.send("GET", "/Users")
    return status, sorted(users, key=lambda u: u.get("Name", "")) if status == 200 and isinstance(users, list) else []


def user_items(server, kind, filters):
    """Each account's {item id: UserData} that the filters return, in name order; a line per refused request."""
    status, users = accounts(server)
    if status != 200:
        return [], [f"{kind} /Users {status}"]
    out, refused = [], []
    for user in users:
        seen = {}
        for name in filters:
            query = urllib.parse.urlencode({"userId": user.get("Id"), "Recursive": "true", "Filters": name})
            status, found = items(server, "/Items?" + query)
            if status != 200:
                refused.append(f"{kind} {user.get('Name')} {name} {status}")
            for item in found:
                seen[item.get("Id")] = item.get("UserData") or {}
        out.append((user.get("Name"), sorted(seen.items())))
    return out, refused


def user_data_lines(server):
    found, out = user_items(server, "userdata", USER_DATA_FILTERS)
    for name, seen in found:
        out += [f"userdata {name} {key} played={flag(data.get('Played'))} fav={flag(data.get('IsFavorite'))} "
                f"resume={data.get('PlaybackPositionTicks') or 0}" for key, data in seen]
    return out


def user_plays_lines(server):
    found, out = user_items(server, "userplays", USER_PLAYS_FILTERS)
    for name, seen in found:
        out += [f"userplays {name} {key} count={data.get('PlayCount') or 0} "
                f"likes={'-' if data.get('Likes') is None else str(data['Likes']).lower()}" for key, data in seen]
    return out


def account_lines(server):
    status, users = accounts(server)
    if status != 200:
        return [f"user /Users {status}"]
    out = []
    for user in users:
        policy, config = user.get("Policy") or {}, user.get("Configuration") or {}
        folders = "all" if policy.get("EnableAllFolders") else len(policy.get("EnabledFolders") or [])
        out.append(f"user {user.get('Name')} admin={flag(policy.get('IsAdministrator'))} "
                   f"disabled={flag(policy.get('IsDisabled'))} hidden={flag(policy.get('IsHidden'))} folders={folders} "
                   f"rating={policy.get('MaxParentalRating')} download={flag(policy.get('EnableContentDownloading'))} "
                   f"sessions={policy.get('MaxActiveSessions')} audio={config.get('AudioLanguagePreference')} "
                   f"subs={config.get('SubtitleMode')} autoplay={flag(config.get('EnableNextEpisodeAutoPlay'))}")
    return out


def login_lines(server):
    status, users = accounts(server)
    present = {u.get("Name") for u in users}
    out = []
    for name, password in sorted(GATE_USERS.items()):
        if name not in present:
            continue
        auth = LOGIN_AUTH.format(name=name)
        # Jellyfin 12 reports every account as having a password, so a wrong one refused is what shows one is kept.
        if password:
            status, _ = server.send("POST", "/Users/AuthenticateByName", {"Username": name, "Pw": "wrong-" + password},
                                    auth=auth)
            out.append(f"badlogin {name} {status}")
        status, body = server.send("POST", "/Users/AuthenticateByName", {"Username": name, "Pw": password}, auth=auth)
        token = "yes" if isinstance(body, dict) and body.get("AccessToken") else "no"
        out.append(f"login {name} {status} token={token}")
        device = DEVICE_ID.format(name=name)
        status, info = server.send("GET", "/Devices/Info?" + urllib.parse.urlencode({"id": device}))
        custom = info.get("CustomName") if status == 200 and isinstance(info, dict) else f"error={status}"
        out.append(f"device {device} name={custom}")
    return out


def preference_lines(server):
    status, users = accounts(server)
    if status != 200:
        return [f"prefs /Users {status}"]
    out = []
    for user in users:
        query = urllib.parse.urlencode({"userId": user.get("Id"), "client": "emby"})
        status, prefs = server.send("GET", "/DisplayPreferences/usersettings?" + query)
        if status != 200 or not isinstance(prefs, dict):
            out.append(f"prefs {user.get('Name')} {status}")
            continue
        custom = ",".join(f"{key}={value}" for key, value in sorted((prefs.get("CustomPrefs") or {}).items()))
        out.append(f"prefs {user.get('Name')} {prefs.get('SortBy')} {prefs.get('SortOrder')} {custom}")
    return out


def server_lines(server):
    """API keys, libraries and settings."""
    status, keys = server.send("GET", "/Auth/Keys")
    names = sorted(row.get("AppName") or "" for row in (keys or {}).get("Items") or []) if status == 200 else []
    out = [f"apikeys {','.join(names) if status == 200 else f'error={status}'}"]
    status, found = items(server, "/Library/MediaFolders")
    out.append(f"libraries {','.join(sorted(row.get('Name') or '' for row in found)) if status == 200 else f'error={status}'}")
    for key, fields in CONFIGURATION.items():
        status, document = server.send("GET", "/System/Configuration" + ("" if key == "system" else f"/{key}"))
        if status != 200 or not isinstance(document, dict):
            out.append(f"config {key} error={status}")
            continue
        out.append(f"config {key} " + " ".join(f"{field}={number(document.get(field))}" for field in fields))
    return out


def number(value):
    """A setting's value; a whole float as an integer, since Ferrofin writes 2.0 where Jellyfin writes 2."""
    return int(value) if isinstance(value, float) and value.is_integer() else value


def image_types(server, item):
    """The types of one item's images. Not their sizes: an uploaded image is stored 0x0 until gl-1054 is fixed,
    which Jellyfin measures after the return."""
    status, images = server.send("GET", f"/Items/{item}/Images")
    if status != 200 or not isinstance(images, list):
        return f"error={status}"
    return ",".join(sorted({row.get("ImageType") or "" for row in images})) or "-"


def movie_lines(server, user_id):
    query = urllib.parse.urlencode({"userId": user_id, "Recursive": "true", "IncludeItemTypes": "Movie",
                                    "SortBy": "SortName", "SortOrder": "Ascending"})
    status, found = items(server, "/Items?" + query)
    if status != 200:
        return [f"item /Items {status}"]
    out = []
    for movie in sorted(found, key=lambda m: (m.get("Name") or "", m.get("Id") or "")):
        status, item = server.send("GET", f"/Items/{movie.get('Id')}?" + urllib.parse.urlencode({"userId": user_id}))
        if status != 200 or not isinstance(item, dict):
            out.append(f"item {movie.get('Name')} error={status}")
            continue
        out.append(f"item {item.get('Name')} lock={flag(item.get('LockData'))} "
                   f"fields={','.join(sorted(item.get('LockedFields') or [])) or '-'} overview={len(item.get('Overview') or '')}")
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
        ok = status == 200 and isinstance(body, dict)
        count = body.get("TotalRecordCount") if ok else f"error={status}"
        out.append(f"collection {item.get('Type')} {count} {item.get('Name')}")
        names = [row.get("Name") or "" for row in body.get("Items") or []] if ok else [f"error={status}"]
        if item.get("Type") != "Playlist":
            names.sort()
        out.append(f"members {item.get('Type')} {item.get('Name')} = {','.join(names)}")
        out.append(f"image {item.get('Type')} {item.get('Name')} {image_types(server, item.get('Id'))}")
    return out


def capture(base_url, data_dir):
    """Every answer, in file order."""
    server = api.Server(base_url, data_dir)
    _, user_id = smoke_user(data_dir)
    return (smoke_lines(base_url, data_dir) + user_data_lines(server) + user_plays_lines(server)
            + account_lines(server) + login_lines(server) + preference_lines(server) + server_lines(server)
            + collection_lines(server, user_id) + movie_lines(server, user_id))


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
    except (OSError, ValueError, KeyError, TypeError, AttributeError, sqlite3.Error) as error:
        print(f"cannot read the copy or write {args.out}: {type(error).__name__}: {error}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
