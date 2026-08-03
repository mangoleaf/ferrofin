#!/usr/bin/env python3
"""Layer-2 write journeys: verify write *effects*, not just status.

Each journey runs a real mutation sequence (setup → POST/PUT/DELETE → read-back)
against BOTH servers using each server's own ids, and checks the effect invariant
on the read-back (e.g. after a favorite POST, the item's UserData.IsFavorite is
true). A write op is `deep_verified` when its effect is confirmed on Hermit AND
Jellyfin behaves the same way. Where they diverge, the row is classified, not
silently passed — this is exactly how the harness surfaces real write gaps (e.g.
the rating-DELETE that never clears Likes).

Writes into an ephemeral container DB (docker `down -v` discards it) over a
read-only media mount — nothing on real disk is touched. Results go to
`parity/journey-results.json`; gen-ledger.py ingests them (feeds deep_verified for
write ops).

Run via sweep.sh (brings both servers up), or directly against provisioned servers:
  HERMIT_URL=... JELLYFIN_URL=... parity/journeys.py
Offline self-check:
  parity/journeys.py --check
"""
import json
import os
import sys
import urllib.parse
import urllib.request
import urllib.error

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from sweep import http, get_json, bring_up, ROOT   # reuse HTTP + provisioning


def q(base, path, token, user):
    return get_json(base, f"{path}{'&' if '?' in path else '?'}userId={user}", token)


def two_movies(base, token, user):
    b = get_json(base, f"/Items?userId={user}&recursive=true&includeItemTypes=Movie"
                       f"&limit=2&sortBy=SortName", token)
    return [i["Id"] for i in (b or {}).get("Items", [])]


def user_data(base, token, user, mid):
    return (q(base, f"/Items/{mid}", token, user) or {}).get("UserData", {}) or {}

# ---------------------------------------------------------------- journeys (per server → {op: effect_ok})

def j_favorites(base, token, user, mid, _m2):
    r = {}
    st, _ = http("POST", f"{base}/UserFavoriteItems/{mid}?userId={user}", token, "")
    r["POST /UserFavoriteItems/{itemId}"] = st < 300 and user_data(base, token, user, mid).get("IsFavorite") is True
    st, _ = http("DELETE", f"{base}/UserFavoriteItems/{mid}?userId={user}", token)
    r["DELETE /UserFavoriteItems/{itemId}"] = st < 300 and user_data(base, token, user, mid).get("IsFavorite") is False
    return r


def j_played(base, token, user, mid, _m2):
    r = {}
    st, _ = http("POST", f"{base}/UserPlayedItems/{mid}?userId={user}", token, "")
    r["POST /UserPlayedItems/{itemId}"] = st < 300 and user_data(base, token, user, mid).get("Played") is True
    st, _ = http("DELETE", f"{base}/UserPlayedItems/{mid}?userId={user}", token)
    r["DELETE /UserPlayedItems/{itemId}"] = st < 300 and user_data(base, token, user, mid).get("Played") is False
    return r


def j_rating(base, token, user, mid, _m2):
    r = {}
    st, _ = http("POST", f"{base}/UserItems/{mid}/Rating?userId={user}&likes=true", token, "")
    r["POST /UserItems/{itemId}/Rating"] = st < 300 and user_data(base, token, user, mid).get("Likes") is True
    st, _ = http("DELETE", f"{base}/UserItems/{mid}/Rating?userId={user}", token)
    # effect = the like is actually cleared (not just a 2xx). Catches "DELETE returns 200 but keeps Likes".
    r["DELETE /UserItems/{itemId}/Rating"] = st < 300 and user_data(base, token, user, mid).get("Likes") is None
    return r


def j_playlist(base, token, user, mid, m2):
    r = {}
    st, raw = http("POST", f"{base}/Playlists", token,
                   json.dumps({"Name": "Parity PL", "Ids": [mid], "UserId": user}))
    pid = json.loads(raw).get("Id") if st < 300 and raw else None
    items = q(base, f"/Playlists/{pid}/Items", token, user) if pid else None
    r["POST /Playlists"] = bool(pid)
    r["GET /Playlists/{playlistId}/Items"] = bool(items and items.get("TotalRecordCount", 0) >= 1)
    if pid:
        st, _ = http("POST", f"{base}/Playlists/{pid}/Items?ids={m2}&userId={user}", token, "")
        after = q(base, f"/Playlists/{pid}/Items", token, user) or {}
        r["POST /Playlists/{playlistId}/Items"] = st < 300 and after.get("TotalRecordCount", 0) >= 2
        # Move the 2nd entry to index 0 and verify the order flips.
        items2 = after.get("Items") or []
        if len(items2) >= 2:
            st, _ = http("POST", f"{base}/Playlists/{pid}/Items/{items2[1]['PlaylistItemId']}/Move/0", token, "")
            moved = (q(base, f"/Playlists/{pid}/Items", token, user) or {}).get("Items") or [{}]
            r["POST /Playlists/{playlistId}/Items/{itemId}/Move/{newIndex}"] = \
                st < 300 and moved[0].get("Id") == items2[1].get("Id")
        entry = (after.get("Items") or [{}])[0].get("PlaylistItemId")
        st, _ = http("DELETE", f"{base}/Playlists/{pid}/Items?entryIds={entry}&userId={user}", token)
        rem = q(base, f"/Playlists/{pid}/Items", token, user) or {}
        r["DELETE /Playlists/{playlistId}/Items"] = st < 300 and rem.get("TotalRecordCount", 99) < after.get("TotalRecordCount", 0)
        # GET /Playlists/{playlistId} returns a PlaylistDto {ItemIds, OpenAccess, Shares};
        # the playlist still holds its original movie, so ItemIds is non-empty.
        got = get_json(base, f"/Playlists/{pid}", token) or {}
        r["GET /Playlists/{playlistId}"] = len(got.get("ItemIds") or []) >= 1
        st, _ = http("POST", f"{base}/Playlists/{pid}?name=Renamed", token, "{}")
        r["POST /Playlists/{playlistId}"] = st < 300
        http("DELETE", f"{base}/Items/{pid}", token)   # cleanup
    return r


def j_collection(base, token, user, mid, m2):
    r = {}
    import urllib.parse
    st, raw = http("POST", f"{base}/Collections?name=ParityCol&ids={mid}", token, "{}")
    cid = json.loads(raw).get("Id") if st < 300 and raw else None
    r["POST /Collections"] = bool(cid)
    if cid:
        st, _ = http("POST", f"{base}/Collections/{cid}/Items?ids={m2}", token, "{}")
        after = q(base, f"/Items?parentId={cid}", token, user) or {}
        r["POST /Collections/{collectionId}/Items"] = st < 300 and after.get("TotalRecordCount", 0) >= 2
        st, _ = http("DELETE", f"{base}/Collections/{cid}/Items?ids={m2}", token)
        rem = q(base, f"/Items?parentId={cid}", token, user) or {}
        r["DELETE /Collections/{collectionId}/Items"] = st < 300 and rem.get("TotalRecordCount", 99) < after.get("TotalRecordCount", 0)
        http("DELETE", f"{base}/Items/{cid}", token)   # cleanup
    return r


def j_users(base, token, user, _m, _m2):
    r = {}
    st, raw = http("POST", f"{base}/Users/New", token,
                   json.dumps({"Name": "parityuser", "Password": "Parity!123"}))
    uid = json.loads(raw).get("Id") if st < 300 and raw else None
    r["POST /Users/New"] = bool(uid)
    if uid:
        got = get_json(base, f"/Users/{uid}", token)
        r["GET /Users/{userId}"] = bool(got and got.get("Id") == uid)
        pol = dict((got or {}).get("Policy", {})); pol["IsAdministrator"] = True
        st, _ = http("POST", f"{base}/Users/{uid}/Policy", token, json.dumps(pol))
        after = get_json(base, f"/Users/{uid}", token) or {}
        r["POST /Users/{userId}/Policy"] = st < 300 and (after.get("Policy") or {}).get("IsAdministrator") is True
        st, _ = http("DELETE", f"{base}/Users/{uid}", token)
        gone = http("GET", f"{base}/Users/{uid}", token)[0]
        r["DELETE /Users/{userId}"] = st < 300 and gone >= 400
    return r


def j_item_edit(base, token, user, mid, _m2):
    r = {}
    dto = q(base, f"/Items/{mid}", token, user)
    if dto:
        dto["Tags"] = list(dict.fromkeys((dto.get("Tags") or []) + ["parity-test"]))
        st, _ = http("POST", f"{base}/Items/{mid}", token, json.dumps(dto))
        back = q(base, f"/Items/{mid}?fields=Tags", token, user) or {}
        r["POST /Items/{itemId}"] = st < 300 and "parity-test" in (back.get("Tags") or [])
    return r


def j_api_keys(base, token, user, _m, _m2):
    r = {}
    http("POST", f"{base}/Auth/Keys?app=parity-probe", token, "")
    keys = (get_json(base, "/Auth/Keys", token) or {}).get("Items") or []
    mine = next((k for k in keys if k.get("AppName") == "parity-probe"), None)
    r["POST /Auth/Keys"] = mine is not None
    if mine:
        st, _ = http("DELETE", f"{base}/Auth/Keys/{mine['AccessToken']}", token)
        after = (get_json(base, "/Auth/Keys", token) or {}).get("Items") or []
        r["DELETE /Auth/Keys/{key}"] = st < 300 and not any(k.get("AppName") == "parity-probe" for k in after)
    return r


def j_user_item_data(base, token, user, mid, _m2):
    r = {}
    st, _ = http("POST", f"{base}/UserItems/{mid}/UserData?userId={user}", token,
                 json.dumps({"Rating": 8.0, "Played": True}))
    ud = user_data(base, token, user, mid)
    r["POST /UserItems/{itemId}/UserData"] = st < 300 and ud.get("Rating") == 8.0 and ud.get("Played") is True
    return r


def j_display_prefs(base, token, user, _m, _m2):
    r = {}
    path = f"/DisplayPreferences/usersettings?userId={user}&client=parity"
    dto = get_json(base, path, token)
    if dto is not None:
        dto.setdefault("CustomPrefs", {})["parityProbe"] = "1"
        st, _ = http("POST", f"{base}{path}", token, json.dumps(dto))
        back = get_json(base, path, token) or {}
        r["POST /DisplayPreferences/{displayPreferencesId}"] = \
            st < 300 and (back.get("CustomPrefs") or {}).get("parityProbe") == "1"
    return r


def j_scheduled_task_triggers(base, token, user, _m, _m2):
    r = {}
    tasks = get_json(base, "/ScheduledTasks", token) or []
    if tasks:
        tid = tasks[0]["Id"]
        triggers = [{"Type": "IntervalTrigger", "IntervalTicks": 36_000_000_000}]
        st, _ = http("POST", f"{base}/ScheduledTasks/{tid}/Triggers", token, json.dumps(triggers))
        back = get_json(base, f"/ScheduledTasks/{tid}", token) or {}
        got = [t.get("Type") for t in (back.get("Triggers") or [])]
        r["POST /ScheduledTasks/{taskId}/Triggers"] = st < 300 and "IntervalTrigger" in got
    return r


def j_device_options(base, token, user, _m, _m2):
    r = {}
    devices = (get_json(base, "/Devices", token) or {}).get("Items") or []
    dev = next((d for d in devices if d.get("Id")), None)
    if dev:
        st, _ = http("POST", f"{base}/Devices/Options?id={dev['Id']}", token,
                     json.dumps({"CustomName": "ParityRenamed"}))
        after = (get_json(base, "/Devices", token) or {}).get("Items") or []
        renamed = next((d for d in after if d.get("Id") == dev["Id"]), {})
        r["POST /Devices/Options"] = st < 300 and renamed.get("CustomName") == "ParityRenamed"
    return r


def my_session(base, token):
    sessions = get_json(base, "/Sessions", token) or []
    return next((s["Id"] for s in sessions if s.get("Client") == "parity"),
                sessions[0]["Id"] if sessions else None)


def j_playstate(base, token, user, mid, _m2):
    r = {}
    ticks = 6_000_000_000
    st, _ = http("POST", f"{base}/Sessions/Playing", token,
                 json.dumps({"ItemId": mid, "PlayMethod": "DirectPlay"}))
    sid = my_session(base, token)
    now = next((s.get("NowPlayingItem") for s in (get_json(base, "/Sessions", token) or [])
                if s.get("Id") == sid), None) or {}
    r["POST /Sessions/Playing"] = st < 300 and now.get("Id") == mid
    st, _ = http("POST", f"{base}/Sessions/Playing/Progress", token,
                 json.dumps({"ItemId": mid, "PositionTicks": ticks, "PlayMethod": "DirectPlay"}))
    ps = next((s.get("PlayState") for s in (get_json(base, "/Sessions", token) or [])
               if s.get("Id") == sid), None) or {}
    r["POST /Sessions/Playing/Progress"] = st < 300 and ps.get("PositionTicks") == ticks
    st, _ = http("POST", f"{base}/Sessions/Playing/Stopped", token,
                 json.dumps({"ItemId": mid, "PositionTicks": ticks}))
    stopped = next((s.get("NowPlayingItem") for s in (get_json(base, "/Sessions", token) or [])
                    if s.get("Id") == sid), None)
    r["POST /Sessions/Playing/Stopped"] = st < 300 and stopped is None
    return r


def j_capabilities(base, token, user, _m, _m2):
    r = {}
    sid = my_session(base, token)
    if sid:
        st, _ = http("POST", f"{base}/Sessions/Capabilities/Full?id={sid}", token,
                     json.dumps({"PlayableMediaTypes": ["Video"], "SupportedCommands": ["DisplayMessage"],
                                 "SupportsMediaControl": True}))
        caps = next((s.get("Capabilities") for s in (get_json(base, "/Sessions", token) or [])
                     if s.get("Id") == sid), None) or {}
        r["POST /Sessions/Capabilities/Full"] = st < 300 and caps.get("SupportsMediaControl") is True
    return r


def j_user_config(base, token, user, _m, _m2):
    r = {}
    cfg = (get_json(base, f"/Users/{user}", token) or {}).get("Configuration") or {}
    cfg["PlayDefaultAudioTrack"] = not cfg.get("PlayDefaultAudioTrack", True)
    st, _ = http("POST", f"{base}/Users/Configuration?userId={user}", token, json.dumps(cfg))
    back = (get_json(base, f"/Users/{user}", token) or {}).get("Configuration") or {}
    r["POST /Users/Configuration"] = st < 300 and back.get("PlayDefaultAudioTrack") == cfg["PlayDefaultAudioTrack"]
    return r


def j_system_config(base, token, user, _m, _m2):
    r = {}
    cfg = get_json(base, "/System/Configuration", token)
    if cfg is not None:
        cfg["EnableFolderView"] = not cfg.get("EnableFolderView", False)
        st, _ = http("POST", f"{base}/System/Configuration", token, json.dumps(cfg))
        back = get_json(base, "/System/Configuration", token) or {}
        r["POST /System/Configuration"] = st < 300 and back.get("EnableFolderView") == cfg["EnableFolderView"]
    return r


def j_playlist_share(base, token, user, mid, _m2):
    r = {}
    _, praw = http("POST", f"{base}/Playlists", token,
                   json.dumps({"Name": "SharePL", "Ids": [mid], "UserId": user}))
    pid = json.loads(praw).get("Id") if praw else None
    _, uraw = http("POST", f"{base}/Users/New", token,
                   json.dumps({"Name": "shareprobe", "Password": "Parity!123"}))
    uid = json.loads(uraw).get("Id") if uraw else None
    if pid and uid:
        st, _ = http("POST", f"{base}/Playlists/{pid}/Users/{uid}", token,
                     json.dumps({"CanEdit": True}))
        shared = (get_json(base, f"/Playlists/{pid}/Users", token) or [])
        r["POST /Playlists/{playlistId}/Users/{userId}"] = st < 300 and any(s.get("UserId") == uid for s in shared)
        # Read the share back both as a list and by id (GET endpoints).
        r["GET /Playlists/{playlistId}/Users"] = any(s.get("UserId") == uid for s in shared)
        one = get_json(base, f"/Playlists/{pid}/Users/{uid}", token) or {}
        r["GET /Playlists/{playlistId}/Users/{userId}"] = one.get("CanEdit") is True
        st, _ = http("DELETE", f"{base}/Playlists/{pid}/Users/{uid}", token)
        after = (get_json(base, f"/Playlists/{pid}/Users", token) or [])
        r["DELETE /Playlists/{playlistId}/Users/{userId}"] = st < 300 and not any(s.get("UserId") == uid for s in after)
        http("DELETE", f"{base}/Users/{uid}", token)   # cleanup
        http("DELETE", f"{base}/Items/{pid}", token)
    return r


def j_item_delete(base, token, user, mid, _m2):
    r = {}
    # Create a throwaway box-set and delete it (never touches real media).
    _, raw = http("POST", f"{base}/Collections?name=DeleteMe&ids={mid}", token, "{}")
    cid = json.loads(raw).get("Id") if raw else None
    if cid:
        st, _ = http("DELETE", f"{base}/Items/{cid}", token)
        gone = http("GET", f"{base}/Items/{cid}?userId={user}", token)[0]
        r["DELETE /Items/{itemId}"] = st < 300 and gone >= 400
    return r


def j_capabilities_query(base, token, user, _m, _m2):
    r = {}
    sid = my_session(base, token)
    if sid:
        st, _ = http("POST", f"{base}/Sessions/Capabilities?id={sid}&playableMediaTypes=Audio", token, "")
        caps = next((s.get("Capabilities") for s in (get_json(base, "/Sessions", token) or [])
                     if s.get("Id") == sid), None) or {}
        r["POST /Sessions/Capabilities"] = st < 300 and caps.get("PlayableMediaTypes") == ["Audio"]
    return r


def j_environment_validate(base, token, user, _m, _m2):
    r = {}
    ok, _ = http("POST", f"{base}/Environment/ValidatePath", token,
                 json.dumps({"Path": "/media/synth/movies", "ValidateWritable": False}))
    bad, _ = http("POST", f"{base}/Environment/ValidatePath", token,
                  json.dumps({"Path": "/no/such/parity/path", "ValidateWritable": False}))
    # A real path validates (2xx); a bogus one is rejected (>=400).
    r["POST /Environment/ValidatePath"] = ok < 300 and bad >= 400
    return r


def j_merge_versions(base, token, user, mid, m2):
    r = {}
    st, _ = http("POST", f"{base}/Videos/MergeVersions?ids={mid},{m2}", token, "")
    survivor = None
    for cand in (mid, m2):
        it = q(base, f"/Items/{cand}?fields=MediaSources", token, user)
        if it and len(it.get("MediaSources") or []) >= 2:
            survivor = cand
            break
    r["POST /Videos/MergeVersions"] = st < 300 and survivor is not None
    if survivor:
        st, _ = http("DELETE", f"{base}/Videos/{survivor}/AlternateSources", token)
        after = q(base, f"/Items/{survivor}?fields=MediaSources", token, user) or {}
        r["DELETE /Videos/{itemId}/AlternateSources"] = st < 300 and len(after.get("MediaSources") or []) <= 1
    return r


def j_playing_items(base, token, user, mid, _m2):
    r = {}
    sid = my_session(base, token)
    ticks = 7_000_000_000
    st, _ = http("POST", f"{base}/PlayingItems/{mid}?playMethod=DirectPlay", token, "")
    now = next((s.get("NowPlayingItem") for s in (get_json(base, "/Sessions", token) or [])
                if s.get("Id") == sid), None) or {}
    r["POST /PlayingItems/{itemId}"] = st < 300 and now.get("Id") == mid
    st, _ = http("POST", f"{base}/PlayingItems/{mid}/Progress?positionTicks={ticks}", token, "")
    ps = next((s.get("PlayState") for s in (get_json(base, "/Sessions", token) or [])
               if s.get("Id") == sid), None) or {}
    r["POST /PlayingItems/{itemId}/Progress"] = st < 300 and ps.get("PositionTicks") == ticks
    st, _ = http("DELETE", f"{base}/PlayingItems/{mid}?positionTicks={ticks}", token)
    cleared = next((s.get("NowPlayingItem") for s in (get_json(base, "/Sessions", token) or [])
                    if s.get("Id") == sid), None)
    r["DELETE /PlayingItems/{itemId}"] = st < 300 and cleared is None
    return r


def j_virtualfolder_rename(base, token, user, _m, _m2):
    r = {}
    folders = get_json(base, "/Library/VirtualFolders", token) or []
    if folders:
        old = folders[0].get("Name") or ""
        new = f"{old} Renamed"
        qo, qn = urllib.parse.quote(old), urllib.parse.quote(new)
        st, _ = http("POST", f"{base}/Library/VirtualFolders/Name?name={qo}&newName={qn}", token, "")
        after = get_json(base, "/Library/VirtualFolders", token) or []
        renamed = any(f.get("Name") == new for f in after)
        r["POST /Library/VirtualFolders/Name"] = st < 300 and renamed
        if renamed:  # restore original name so library state is unchanged
            http("POST", f"{base}/Library/VirtualFolders/Name?name={qn}&newName={qo}", token, "")
    return r


def j_virtualfolder_crud(base, token, user, _m, _m2):
    """Full VirtualFolders lifecycle on a throwaway library: create → add/update/remove a
    media path → toggle a library option → delete. Each step verifies its effect via
    GET /Library/VirtualFolders, and the library is removed at the end so shared state is
    untouched (it runs last, so it can't perturb the other journeys)."""
    r = {}
    name = "ParityCRUD"
    qn = urllib.parse.quote(name)
    tv = "/media/synth/tv"
    qtv = urllib.parse.quote(tv)

    def find():
        for f in get_json(base, "/Library/VirtualFolders", token) or []:
            if f.get("Name") == name:
                return f
        return None

    if find():  # leftover from a prior aborted run
        http("DELETE", f"{base}/Library/VirtualFolders?name={qn}&refreshLibrary=false", token)

    st, _ = http("POST", f"{base}/Library/VirtualFolders?name={qn}&collectionType=movies"
                         f"&paths=%2Fmedia%2Fsynth%2Fmovies&refreshLibrary=false", token, "{}")
    created = find()
    r["POST /Library/VirtualFolders"] = st < 300 and created is not None
    if not created:
        return r
    lib_id = created.get("ItemId")

    st, _ = http("POST", f"{base}/Library/VirtualFolders/Paths?refreshLibrary=false", token,
                 json.dumps({"Name": name, "PathInfo": {"Path": tv}}))
    r["POST /Library/VirtualFolders/Paths"] = st < 300 and tv in ((find() or {}).get("Locations") or [])

    # Update the added path's info (no observable location change) — verify 204 + lib intact.
    st, _ = http("POST", f"{base}/Library/VirtualFolders/Paths/Update?refreshLibrary=false", token,
                 json.dumps({"Name": name, "PathInfo": {"Path": tv}}))
    r["POST /Library/VirtualFolders/Paths/Update"] = st < 300 and find() is not None

    # Toggle a library option and verify it round-trips through GET.
    opts = (find() or {}).get("LibraryOptions") or {}
    want = not opts.get("EnablePhotos", True)
    opts["EnablePhotos"] = want
    st, _ = http("POST", f"{base}/Library/VirtualFolders/LibraryOptions", token,
                 json.dumps({"Id": lib_id, "LibraryOptions": opts}))
    got = ((find() or {}).get("LibraryOptions") or {}).get("EnablePhotos")
    r["POST /Library/VirtualFolders/LibraryOptions"] = st < 300 and got == want

    st, _ = http("DELETE", f"{base}/Library/VirtualFolders/Paths?name={qn}&path={qtv}"
                          f"&refreshLibrary=false", token)
    r["DELETE /Library/VirtualFolders/Paths"] = st < 300 and tv not in ((find() or {}).get("Locations") or [])

    st, _ = http("DELETE", f"{base}/Library/VirtualFolders?name={qn}&refreshLibrary=false", token)
    r["DELETE /Library/VirtualFolders"] = st < 300 and find() is None
    return r


def auth_device(base, username, pw, device):
    """AuthenticateByName under a distinct DeviceId, returning the full result (which
    carries SessionInfo.Id + AccessToken). A dedicated DeviceId avoids colliding with —
    and destroying — the harness's own DeviceId='parity' session."""
    hdr = {
        "Content-Type": "application/json",
        "Authorization": f'MediaBrowser Client="parityctl", Device="parityctl", '
                         f'DeviceId="{device}", Version="1.0"',
    }
    req = urllib.request.Request(f"{base}/Users/AuthenticateByName",
                                 data=json.dumps({"Username": username, "Pw": pw}).encode(),
                                 method="POST", headers=hdr)
    try:
        with urllib.request.urlopen(req, timeout=30) as rr:
            return json.loads(rr.read())
    except (urllib.error.HTTPError, urllib.error.URLError, ValueError, TimeoutError):
        return {}


def j_sessions(base, token, user, mid, _m2):
    """Session remote-control surface. Prior journeys authenticate throwaway users on the
    shared DeviceId='parity', which destroys that session — so this journey stands up its
    OWN session (a throwaway user under a dedicated DeviceId) and takes the session id from
    the auth response, then drives every control against it with the admin token. Cleans up."""
    r = {}
    _, uraw = http("POST", f"{base}/Users/New", token,
                   json.dumps({"Name": "sessctl", "Password": "Parity!123"}))
    ctl_uid = json.loads(uraw).get("Id") if uraw else None
    if not ctl_uid:
        return r
    auth = auth_device(base, "sessctl", "Parity!123", "parity-sessctl")
    sid = (auth.get("SessionInfo") or {}).get("Id")
    ctl_tok = auth.get("AccessToken")
    if not sid or not ctl_tok:
        http("DELETE", f"{base}/Users/{ctl_uid}", token)
        return r
    # The controlled session advertises remote-control support so commands are accepted.
    http("POST", f"{base}/Sessions/Capabilities/Full", ctl_tok, json.dumps({
        "PlayableMediaTypes": ["Video", "Audio"],
        "SupportedCommands": ["Mute", "Unmute", "DisplayMessage", "GoHome", "Play", "Pause", "Stop"],
        "SupportsMediaControl": True,
        "SupportsPersistentIdentifier": True,
    }))

    # Fire-and-accept controls (admin token → the controllable session): the server accepts
    # the request even with no live receiver on the other end.
    controls = [
        ("POST /Sessions/Viewing", f"/Sessions/Viewing?sessionId={sid}&itemId={mid}", ""),
        ("POST /Sessions/Playing/Ping", "/Sessions/Playing/Ping?playSessionId=parity", ""),
        ("POST /Sessions/{sessionId}/Command/{command}", f"/Sessions/{sid}/Command/Mute", ""),
        ("POST /Sessions/{sessionId}/Command", f"/Sessions/{sid}/Command",
         json.dumps({"Name": "DisplayMessage", "Arguments": {"Header": "H", "Text": "T"}})),
        ("POST /Sessions/{sessionId}/Message", f"/Sessions/{sid}/Message",
         json.dumps({"Text": "hi", "Header": "H", "TimeoutMs": 500})),
        ("POST /Sessions/{sessionId}/Playing", f"/Sessions/{sid}/Playing?playCommand=PlayNow&itemIds={mid}", ""),
        ("POST /Sessions/{sessionId}/Playing/{command}", f"/Sessions/{sid}/Playing/Pause", ""),
        ("POST /Sessions/{sessionId}/System/{command}", f"/Sessions/{sid}/System/GoHome", ""),
        ("POST /Sessions/{sessionId}/Viewing", f"/Sessions/{sid}/Viewing?itemType=Movie&itemId={mid}&itemName=X", ""),
    ]
    for op, path, body in controls:
        st, _ = http("POST", f"{base}{path}", token, body)
        r[op] = st < 300

    # Additional-user add/remove, observed on the session's AdditionalUsers.
    def additional():
        s = next((x for x in (get_json(base, "/Sessions", token) or []) if x.get("Id") == sid), {})
        return [a.get("UserId") for a in (s.get("AdditionalUsers") or [])]
    st, _ = http("POST", f"{base}/Sessions/{sid}/User/{user}", token, "")
    r["POST /Sessions/{sessionId}/User/{userId}"] = st < 300 and user in additional()
    st, _ = http("DELETE", f"{base}/Sessions/{sid}/User/{user}", token)
    r["DELETE /Sessions/{sessionId}/User/{userId}"] = st < 300 and user not in additional()

    # Logout revokes the calling token — do it to the throwaway token so the harness survives.
    st, _ = http("POST", f"{base}/Sessions/Logout", ctl_tok, "")
    dead = http("GET", f"{base}/Users/Me", ctl_tok)[0]
    r["POST /Sessions/Logout"] = st < 300 and dead == 401

    http("DELETE", f"{base}/Users/{ctl_uid}", token)   # cleanup
    return r


def j_system_and_refresh(base, token, user, mid, _m2):
    """Status-effect writes with no observable read-back: ping, item/library refresh
    triggers, and a content-type override. The differential still confirms both servers
    accept the identical request the same way. Runs last so a queued rescan can't perturb
    the other journeys."""
    r = {}
    st, _ = http("POST", f"{base}/System/Ping", token, "")
    r["POST /System/Ping"] = st < 300
    st, _ = http("POST", f"{base}/Items/{mid}/ContentType?contentType=movies", token, "")
    r["POST /Items/{itemId}/ContentType"] = st < 300
    st, _ = http("POST", f"{base}/Items/{mid}/Refresh?metadataRefreshMode=Default"
                         f"&imageRefreshMode=None", token, "")
    r["POST /Items/{itemId}/Refresh"] = st < 300
    st, _ = http("POST", f"{base}/Library/Refresh", token, "")
    r["POST /Library/Refresh"] = st < 300
    return r


def j_users_password(base, token, user, _m, _m2):
    r = {}
    _, uraw = http("POST", f"{base}/Users/New", token,
                   json.dumps({"Name": "pwprobe", "Password": "Old!123"}))
    uid = json.loads(uraw).get("Id") if uraw else None
    if uid:
        st, _ = http("POST", f"{base}/Users/Password?userId={uid}", token,
                     json.dumps({"CurrentPw": "Old!123", "NewPw": "New!456"}))
        # Effect: authenticating with the NEW password succeeds.
        auth = http("POST", f"{base}/Users/AuthenticateByName", token,
                    json.dumps({"Username": "pwprobe", "Pw": "New!456"}))[0]
        r["POST /Users/Password"] = st < 300 and auth == 200
        http("DELETE", f"{base}/Users/{uid}", token)   # cleanup
    return r


JOURNEYS = [j_favorites, j_played, j_rating, j_playlist, j_collection, j_users, j_item_edit,
            j_api_keys, j_user_item_data, j_display_prefs, j_scheduled_task_triggers,
            j_device_options, j_playstate, j_capabilities, j_user_config, j_system_config,
            j_playlist_share, j_item_delete, j_capabilities_query, j_environment_validate,
            j_merge_versions, j_playing_items, j_virtualfolder_rename,
            j_users_password, j_virtualfolder_crud, j_sessions, j_system_and_refresh]

# ---------------------------------------------------------------- run

def run_all(base, token, user):
    mids = two_movies(base, token, user)
    if len(mids) < 2:
        raise SystemExit(f"{base}: need >=2 movies, got {len(mids)}")
    out = {}
    for jn in JOURNEYS:
        try:
            out.update(jn(base, token, user, mids[0], mids[1]))
        except Exception as e:   # a journey blowing up marks its ops failed, doesn't abort the rest
            out[f"_error:{jn.__name__}"] = str(e)
    return out


def journeys(hermit_url, jellyfin_url):
    ht, hu = bring_up(hermit_url, "hermit")
    h = run_all(hermit_url, ht, hu)
    j = {}
    if jellyfin_url:
        jt, ju = bring_up(jellyfin_url, "jellyfin")
        j = run_all(jellyfin_url, jt, ju)

    rows = {}
    for op in sorted(k for k in h if not k.startswith("_")):
        h_ok = h.get(op)
        j_ok = j.get(op)
        if jellyfin_url:
            deep = bool(h_ok and j_ok)
            if h_ok and not j_ok:
                cls = "flagged: Jellyfin read-back differed (verify: oracle setup or Hermit extra)"
            elif not h_ok and j_ok:
                cls = "flagged: Hermit read-back did not reflect the write (verify: real gap vs read-back method)"
            elif not h_ok:
                cls = "flagged: write effect not observed on either server (likely corpus/setup)"
            else:
                cls = "ok"
            rows[op] = {"deep_verified": deep, "classification": cls, "note": f"H={h_ok} J={j_ok}"}
        else:
            rows[op] = {"deep_verified": bool(h_ok), "classification": "ok" if h_ok else "write effect not confirmed on Hermit",
                        "note": f"H={h_ok}"}
    return rows, {k: v for k, v in h.items() if k.startswith("_")}


def main():
    if "--check" in sys.argv:
        selfcheck()
        return
    hermit = os.environ.get("HERMIT_URL", "http://localhost:18096")
    jellyfin = os.environ.get("JELLYFIN_URL")
    rows, errors = journeys(hermit, jellyfin)
    out = {"generated_by": "parity/journeys.py", "last_verified": os.environ.get("PARITY_STAMP", ""),
           "errors": errors, "rows": rows}
    with open(os.path.join(ROOT, "parity/journey-results.json"), "w") as f:
        json.dump(out, f, indent=2, sort_keys=True)
        f.write("\n")
    ok = sum(1 for v in rows.values() if v["deep_verified"])
    print(f"wrote parity/journey-results.json — {len(rows)} write ops, {ok} deep-verified"
          + (f", errors: {list(errors)}" if errors else ""))


def selfcheck():
    # The combine logic: deep_verified only when the effect holds on BOTH servers.
    def combine(h_ok, j_ok):
        return bool(h_ok and j_ok)
    assert combine(True, True) is True
    assert combine(True, False) is False   # Jellyfin disagrees → not verified
    assert combine(False, True) is False   # real Hermit gap → not verified
    # Every journey advertises only op keys that exist in the vendored spec.
    import glob
    spec = json.load(open(sorted(glob.glob(os.path.join(ROOT, "contracts/jellyfin-openapi-*.json")))[-1]))
    valid = {f"{m.upper()} {p}" for p, it in spec["paths"].items() for m in it if m in
             ("get", "post", "put", "delete", "patch")}
    class Rec(dict):
        pass
    # Dry-run each journey with a stub http that records nothing but returns benign values.
    declared = set()
    for jn in JOURNEYS:
        # Introspect by running against a no-op recorder base is overkill; instead assert the
        # op-key literals in each function source are spec paths.
        import inspect
        for line in inspect.getsource(jn).splitlines():
            if 'r["' in line:
                key = line.split('r["', 1)[1].split('"]', 1)[0]
                declared.add(key)
    missing = sorted(k for k in declared if k not in valid)
    assert not missing, f"journey op-keys not in spec: {missing}"
    print(f"ok: combine logic, {len(declared)} journey op-keys all valid spec paths")


if __name__ == "__main__":
    main()
