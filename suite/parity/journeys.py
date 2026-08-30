#!/usr/bin/env python3
"""Layer-2 write journeys: verify write *effects*, not just status.

Each journey runs a real mutation sequence (setup → POST/PUT/DELETE → read-back)
against BOTH servers using each server's own ids, and checks the effect invariant
on the read-back (e.g. after a favorite POST, the item's UserData.IsFavorite is
true). A write op is `deep_verified` when its effect is confirmed on Ferrofin AND
Jellyfin behaves the same way. Where they diverge, the row is classified, not
silently passed — this is exactly how the harness surfaces real write gaps (e.g.
the rating-DELETE that never clears Likes).

Writes into an ephemeral container DB (docker `down -v` discards it) over a
read-only media mount — nothing on real disk is touched. Results go to
`parity/journey-results.json`; gen-ledger.py ingests them (feeds deep_verified for
write ops).

Run via sweep.sh (brings both servers up), or directly against provisioned servers:
  FERROFIN_URL=... JELLYFIN_URL=... parity/journeys.py
Offline self-check:
  parity/journeys.py --check
"""
import json
import os
import sys
import time
import urllib.parse
import urllib.request
import urllib.error

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from sweep import http, get_json, bring_up, container_read, ROOT, USER, PASS, CLIENT, opensubtitles_credentials   # reuse HTTP + provisioning


class Same:
    """A journey step whose effect held on this server AND whose `evidence` must equal
    the other server's.

    A plain bool only asserts per-server self-consistency: each server is checked against
    what *it* was posted, so two servers that each faithfully round-trip *different*
    defaults both pass. That is a real hole — the `EnableRealtimeMonitor` default
    (Ferrofin true / Jellyfin false on a freshly created library) had to be found by hand
    because the LibraryOptions row asserted only the round-trip. Returning `Same(ok,
    evidence)` instead makes the runner compare the two servers' evidence as well, so the
    row is only green when the write took AND both servers ended up in the same state.

    `evidence` must be a value that is genuinely comparable across two independent
    instances — a settings object, a flag, a count. Never an id, a date, or anything
    per-instance; those belong nowhere near a cross-server equality.
    """

    __slots__ = ("ok", "evidence")

    def __init__(self, ok, evidence):
        self.ok = bool(ok)
        self.evidence = evidence

    def __bool__(self):
        return self.ok

    def __repr__(self):
        return f"{self.ok}(+evidence)"


def cross_server_ok(h_ok, j_ok):
    """True unless BOTH sides returned `Same` and their evidence disagrees.

    This is the whole cross-server rule, in one place, so the self-check exercises the
    code the runner actually runs rather than a restatement of it."""
    if isinstance(h_ok, Same) and isinstance(j_ok, Same):
        return h_ok.evidence == j_ok.evidence
    return True


def evidence_diff(h, j):
    """A short human note naming where two evidence values differ. Dicts are reported as
    the keys whose values disagree (with both values); anything else as a repr pair."""
    if isinstance(h, dict) and isinstance(j, dict):
        keys = sorted(set(h) | set(j))
        bad = [f"{k}: H={h.get(k)!r} J={j.get(k)!r}" for k in keys if h.get(k) != j.get(k)]
        return "; ".join(bad) or "(no key differs)"
    return f"H={h!r} J={j!r}"


def q(base, path, token, user):
    return get_json(base, f"{path}{'&' if '?' in path else '?'}userId={user}", token)


def two_movies(base, token, user):
    b = get_json(base, f"/Items?userId={user}&recursive=true&includeItemTypes=Movie"
                       f"&limit=2&sortBy=SortName", token)
    return [i["Id"] for i in (b or {}).get("Items", [])]


def user_data(base, token, user, mid):
    return (q(base, f"/Items/{mid}", token, user) or {}).get("UserData", {}) or {}

# ---------------------------------------------------------------- journeys (per server → {op: effect_ok})

def j_startup(base, token, user, _m, _m2):
    """The first-run wizard endpoints. Setup is complete on both servers by the time journeys
    run, but the controller's policy is FirstTimeSetupOrElevated — an admin can drive it after
    setup — so each POST is replayed with exactly the values the harness provisioned and its
    effect confirmed on the read-back (nothing actually changes). Must run FIRST: Startup/User
    rewrites "the first user", which is only the admin while no throwaway users exist."""
    r = {}
    cfg = {"UICulture": "en-US", "MetadataCountryCode": "US", "PreferredMetadataLanguage": "en"}
    st, _ = http("POST", f"{base}/Startup/Configuration", token, json.dumps(cfg))
    back = get_json(base, "/Startup/Configuration", token) or {}
    r["POST /Startup/Configuration"] = st < 300 and all(back.get(k) == v for k, v in cfg.items())
    # Post-setup the first user already has a password, and the contract is 403 (the Forbid
    # guard upstream added in v12, which Ferrofin ports). Jellyfin 10.11.8 predates it and
    # silently re-sets the admin password instead — sending the provisioned credentials keeps
    # that a no-op. Classified in classifications.json; this asserts the correct contract.
    # Jellyfin picks `Users.First()` from an unordered dictionary, so the call is only safe
    # while the admin is the ONLY user — a stray user from a failed cleanup would be the one
    # renamed/re-passworded instead. Guarded rather than assumed.
    if len(get_json(base, "/Users", token) or []) == 1:
        st, _ = http("POST", f"{base}/Startup/User", token, json.dumps({"Name": USER, "Password": PASS}))
        back = get_json(base, "/Startup/User", token) or {}
        r["POST /Startup/User"] = st == 403 and back.get("Name") == USER
    else:
        r["POST /Startup/User"] = False   # not attempted: more than one user on the instance
    st, _ = http("POST", f"{base}/Startup/RemoteAccess", token,
                 json.dumps({"EnableRemoteAccess": True, "EnableAutomaticPortMapping": False}))
    net = get_json(base, "/System/Configuration/network", token) or {}
    r["POST /Startup/RemoteAccess"] = st < 300 and net.get("EnableRemoteAccess") is True
    st, _ = http("POST", f"{base}/Startup/Complete", token, "")
    pub = get_json(base, "/System/Info/Public", None) or {}
    r["POST /Startup/Complete"] = st < 300 and pub.get("StartupWizardCompleted") is True
    return r


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
        # Read the options back by id (needs a device that has options set — just did).
        opts = get_json(base, f"/Devices/Options?id={dev['Id']}", token) or {}
        r["GET /Devices/Options"] = opts.get("CustomName") == "ParityRenamed"
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

    # Toggle a library option and verify it round-trips through GET. Keyed by `Id` alone,
    # which is the only key a real client has (`UpdateLibraryOptionsDto` is `{Guid Id,
    # LibraryOptions}` — no Name), and asserted on the WHOLE options object, not just the
    # flipped flag: C# replaces the options wholesale, so anything the server silently
    # drops or rewrites on the way through is a divergence this row should catch.
    #
    # The read-back is ALSO handed to the runner as cross-server evidence. The round-trip
    # alone is per-server self-consistency — each server is compared against what it was
    # posted, and the posted object is derived from that same server's own read — so two
    # servers whose LibraryOptions *defaults* differ would both pass it. Comparing the
    # resulting objects is what catches a default divergence (both libraries here are
    # created by the same request, so their options must agree key for key).
    opts = (find() or {}).get("LibraryOptions") or {}
    opts["EnablePhotos"] = not opts.get("EnablePhotos", True)
    st, _ = http("POST", f"{base}/Library/VirtualFolders/LibraryOptions", token,
                 json.dumps({"Id": lib_id, "LibraryOptions": opts}))
    got = (find() or {}).get("LibraryOptions")
    r["POST /Library/VirtualFolders/LibraryOptions"] = Same(st < 300 and got == opts, got)

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


def j_config_writes(base, token, user, _m, _m2):
    """Branding config round-trips through both the dedicated endpoint and the generic
    keyed endpoint, read back via GET /System/Configuration/{key}. Restores it after."""
    r = {}
    disc = "Parity disclaimer"
    st, _ = http("POST", f"{base}/System/Configuration/Branding", token,
                 json.dumps({"LoginDisclaimer": disc, "CustomCss": "", "SplashscreenEnabled": False}))
    got = get_json(base, "/System/Configuration/branding", token) or {}
    r["POST /System/Configuration/Branding"] = st < 300 and got.get("LoginDisclaimer") == disc
    r["GET /System/Configuration/{key}"] = got.get("LoginDisclaimer") == disc
    disc2 = "Parity disclaimer 2"
    st, _ = http("POST", f"{base}/System/Configuration/branding", token,
                 json.dumps({"LoginDisclaimer": disc2, "CustomCss": "", "SplashscreenEnabled": False}))
    got2 = get_json(base, "/System/Configuration/branding", token) or {}
    r["POST /System/Configuration/{key}"] = st < 300 and got2.get("LoginDisclaimer") == disc2
    http("POST", f"{base}/System/Configuration/Branding", token,   # restore
         json.dumps({"LoginDisclaimer": "", "CustomCss": "", "SplashscreenEnabled": False}))
    return r


def j_scheduled_run(base, token, user, _m, _m2):
    """Start then stop a scheduled task. Picks a task that is not a heavy library
    scan/refresh so the immediate stop leaves nothing running."""
    r = {}
    tasks = get_json(base, "/ScheduledTasks", token) or []
    heavy = ("scan", "refresh", "library", "metadata", "thumbnail", "trickplay")
    task = next((t for t in tasks if not any(h in (t.get("Name", "").lower()) for h in heavy)), None)
    task = task or (tasks[0] if tasks else None)
    tid = task.get("Id") if task else None
    if tid:
        st, _ = http("POST", f"{base}/ScheduledTasks/Running/{tid}", token, "")
        r["POST /ScheduledTasks/Running/{taskId}"] = st < 300
        st, _ = http("DELETE", f"{base}/ScheduledTasks/Running/{tid}", token)
        r["DELETE /ScheduledTasks/Running/{taskId}"] = st < 300
    return r


def j_playbackinfo_post(base, token, user, mid, _m2):
    st, raw = http("POST", f"{base}/Items/{mid}/PlaybackInfo?userId={user}", token, "{}")
    ok = False
    if st < 300 and raw:
        try:
            ok = len(json.loads(raw).get("MediaSources") or []) >= 1
        except ValueError:
            ok = False
    return {"POST /Items/{itemId}/PlaybackInfo": ok}


def j_active_encodings(base, token, user, _m, _m2):
    # No live transcode for this device/session → an idempotent 2xx no-op on both servers.
    st, _ = http("DELETE", f"{base}/Videos/ActiveEncodings?deviceId=parity&playSessionId=none", token)
    return {"DELETE /Videos/ActiveEncodings": st < 300}


def j_clientlog(base, token, user, _m, _m2):
    st, _ = http("POST", f"{base}/ClientLog/Document", token, "parity client log line")
    return {"POST /ClientLog/Document": st < 300}


def j_authenticate(base, token, user, _m, _m2):
    """Authenticate a throwaway user by name and confirm an access token comes back."""
    r = {}
    _, uraw = http("POST", f"{base}/Users/New", token,
                   json.dumps({"Name": "authprobe", "Password": "Parity!123"}))
    uid = json.loads(uraw).get("Id") if uraw else None
    if uid:
        _, araw = http("POST", f"{base}/Users/AuthenticateByName", None,
                       json.dumps({"Username": "authprobe", "Pw": "Parity!123"}))
        tok2 = json.loads(araw).get("AccessToken") if araw else None
        r["POST /Users/AuthenticateByName"] = bool(tok2)
        http("DELETE", f"{base}/Users/{uid}", token)
    return r


def j_user_update(base, token, user, _m, _m2):
    """Update a throwaway user via POST /Users?userId= and read the change back."""
    r = {}
    _, uraw = http("POST", f"{base}/Users/New", token,
                   json.dumps({"Name": "updprobe", "Password": "Parity!123"}))
    uid = json.loads(uraw).get("Id") if uraw else None
    if uid:
        u = get_json(base, f"/Users/{uid}", token) or {}
        u["Name"] = "updprobe2"
        st, _ = http("POST", f"{base}/Users?userId={uid}", token, json.dumps(u))
        after = get_json(base, f"/Users/{uid}", token) or {}
        r["POST /Users"] = st < 300 and after.get("Name") == "updprobe2"
        http("DELETE", f"{base}/Users/{uid}", token)
    return r


def j_devices_delete(base, token, user, _m, _m2):
    """Register a device via a throwaway login under a dedicated DeviceId, then delete it
    and confirm it drops off GET /Devices — without touching the harness's own device."""
    r = {}
    _, uraw = http("POST", f"{base}/Users/New", token,
                   json.dumps({"Name": "delprobe", "Password": "Parity!123"}))
    uid = json.loads(uraw).get("Id") if uraw else None
    if uid:
        auth = auth_device(base, "delprobe", "Parity!123", "parity-deldev")
        if auth.get("AccessToken"):
            def has_dev():
                items = (get_json(base, "/Devices", token) or {}).get("Items") or []
                return any(d.get("Id") == "parity-deldev" for d in items)
            present = has_dev()
            st, _ = http("DELETE", f"{base}/Devices?id=parity-deldev", token)
            r["DELETE /Devices"] = present and st < 300 and not has_dev()
        http("DELETE", f"{base}/Users/{uid}", token)
    return r


def j_bulk_item_delete(base, token, user, mid, _m2):
    """DELETE /Items (bulk, by ids query) — delete a throwaway playlist item and confirm
    it is gone. Uses a created playlist so no fixture media is touched."""
    r = {}
    _, praw = http("POST", f"{base}/Playlists", token,
                   json.dumps({"Name": "BulkDelPL", "Ids": [mid], "UserId": user}))
    pid = json.loads(praw).get("Id") if praw else None
    if pid:
        # Both servers accept the bulk delete. The removal *effect* is deep-verified by the
        # singular DELETE /Items/{itemId}; a read-back here is unreliable because Jellyfin
        # deletes asynchronously (the item lingers a beat) while Ferrofin deletes synchronously.
        st, _ = http("DELETE", f"{base}/Items?ids={pid}", token)
        r["DELETE /Items"] = st < 300
        http("DELETE", f"{base}/Items/{pid}", token)  # ensure cleanup even if bulk lagged
    return r


SUBTITLE_REFRESH_WAIT_S = 10   # Jellyfin lists an uploaded stream only once its refresh ran


def external_subtitles(base, token, user, mid):
    """(index, language) of the item's EXTERNAL subtitle streams (uploaded files)."""
    item = q(base, f"/Items/{mid}?fields=MediaStreams", token, user) or {}
    return [(s["Index"], s.get("Language")) for s in item.get("MediaStreams") or []
            if s.get("Type") == "Subtitle" and s.get("IsExternal") and "Index" in s]


def external_subtitle_indexes(base, token, user, mid):
    return [i for i, _ in external_subtitles(base, token, user, mid)]


def j_subtitles_upload(base, token, user, mid, _m2):
    """Upload an external subtitle → it appears as an external subtitle stream → delete it by
    index → it is gone. (Delete only ever targets external streams; the embedded eng track
    the fixture carries stays.) The upload is "fra", and every external fra stream is reaped
    at the end whatever happened, so a stale file from an aborted run cannot mask the next."""
    import base64
    r = {}
    before = set(external_subtitle_indexes(base, token, user, mid))
    srt = "1\n00:00:00,000 --> 00:00:01,000\nParity\n"
    body = json.dumps({
        "Language": "fra", "Format": "srt", "IsForced": False,
        "IsHearingImpaired": False,
        "Data": base64.b64encode(srt.encode()).decode(),
    })
    st, _ = http("POST", f"{base}/Videos/{mid}/Subtitles", token, body)
    added = []
    for _ in range(SUBTITLE_REFRESH_WAIT_S):
        added = [i for i in external_subtitle_indexes(base, token, user, mid) if i not in before]
        if added:
            break
        time.sleep(1)
    r["POST /Videos/{itemId}/Subtitles"] = st < 300 and bool(added)
    if added:
        st, _ = http("DELETE", f"{base}/Videos/{mid}/Subtitles/{added[0]}", token)
        gone = added[0] not in external_subtitle_indexes(base, token, user, mid)
        r["DELETE /Videos/{itemId}/Subtitles/{index}"] = st < 300 and gone
    for i, lang in external_subtitles(base, token, user, mid):   # reap, whichever path ran
        if lang == "fra":
            http("DELETE", f"{base}/Videos/{mid}/Subtitles/{i}", token)
    return r


def j_merge_versions_controller(base, token, user, mid, m2):
    """The /MergeVersions/* controller (no documented params/body). Probe each with the
    ids query the sibling /Videos/MergeVersions uses; records acceptance so the differential
    reveals whether both servers implement it identically."""
    r = {}
    for op, path in [
        ("POST /MergeVersions/MergeMovies", f"/MergeVersions/MergeMovies?ids={mid},{m2}"),
        ("POST /MergeVersions/MergeEpisodes", f"/MergeVersions/MergeEpisodes?ids={mid},{m2}"),
        ("POST /MergeVersions/SplitMovies", f"/MergeVersions/SplitMovies?id={mid}"),
        ("POST /MergeVersions/SplitEpisodes", f"/MergeVersions/SplitEpisodes?id={mid}"),
    ]:
        st, _ = http("POST", f"{base}{path}", token, "")
        r[op] = st < 300
    return r


def j_quickconnect(base, token, user, _m, _m2):
    """Full QuickConnect handshake: the device initiates → the admin authorizes the code →
    the device polls Connect (now authenticated) → the device exchanges the secret for an
    access token. The device runs under its OWN DeviceId so that exchanging the secret
    issues a token for *its* session, not the harness's DeviceId='parity' session (which
    the rest of the run still needs)."""
    r = {}
    dev_hdr = {
        "Content-Type": "application/json",
        "Authorization": f'MediaBrowser Token="{token}", Client="parityqc", '
                         f'Device="parityqc", DeviceId="parity-qc", Version="1.0"',
    }

    def dev(method, path, body=None):
        req = urllib.request.Request(base + path, data=(body.encode() if body else None),
                                     method=method, headers=dev_hdr)
        try:
            with urllib.request.urlopen(req, timeout=30) as rr:
                return rr.status, rr.read()
        except urllib.error.HTTPError as e:
            return e.code, e.read()
        except (urllib.error.URLError, TimeoutError, ConnectionError):
            return 0, b""

    st, raw = dev("POST", "/QuickConnect/Initiate")
    init = json.loads(raw) if st < 300 and raw else {}
    secret, code = init.get("Secret"), init.get("Code")
    r["POST /QuickConnect/Initiate"] = bool(secret and code)
    if not (secret and code):
        return r
    # The admin authorizes the code with the harness token (an admin action).
    st, _ = http("POST", f"{base}/QuickConnect/Authorize?code={code}&userId={user}", token, "")
    r["POST /QuickConnect/Authorize"] = st < 300
    # The device polls Connect with its secret; after authorize it is Authenticated.
    st, raw = dev("GET", f"/QuickConnect/Connect?secret={secret}")
    conn = json.loads(raw) if st < 300 and raw else {}
    r["GET /QuickConnect/Connect"] = conn.get("Authenticated") is True
    st, raw = dev("POST", "/Users/AuthenticateWithQuickConnect", json.dumps({"Secret": secret}))
    tok2 = json.loads(raw).get("AccessToken") if st < 300 and raw else None
    r["POST /Users/AuthenticateWithQuickConnect"] = bool(tok2)
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


def j_forgot_password(base, token, user, _m, _m2):
    """The local password-reset flow on a throwaway user. ForgotPassword answers with a PIN
    challenge whose PIN is written to a file on the server host; the harness reads that file
    out of the container (docker compose exec) and redeems it. The effect (port of
    DefaultPasswordResetProvider.RedeemPasswordResetPin) is that the PIN BECOMES the user's
    password, so the read-back is a login with it. Requests come from the docker bridge (a
    private range), which both servers treat as local — the gate for this flow."""
    r = {}
    # A stale fpprobe from an aborted run would make Users/New fail and leave both ops
    # silently untested: remove it first.
    for u in get_json(base, "/Users", token) or []:
        if u.get("Name") == "fpprobe":
            http("DELETE", f"{base}/Users/{u['Id']}", token)
    _, uraw = http("POST", f"{base}/Users/New", token,
                   json.dumps({"Name": "fpprobe", "Password": "Fp!123"}))
    uid = json.loads(uraw).get("Id") if uraw else None
    if not uid:
        return r
    st, raw = http("POST", f"{base}/Users/ForgotPassword", None,
                   json.dumps({"EnteredUsername": "fpprobe"}))
    try:
        challenge = json.loads(raw)
    except ValueError:
        challenge = {}
    pin_file = challenge.get("PinFile") or ""
    r["POST /Users/ForgotPassword"] = (st == 200 and challenge.get("Action") == "PinCode"
                                       and bool(pin_file) and bool(challenge.get("PinExpirationDate")))
    pin = None
    contents = container_read(base, pin_file) if pin_file else None
    if contents:
        try:
            pin = json.loads(contents).get("Pin")
        except ValueError:
            pin = None
    if pin:
        st, raw = http("POST", f"{base}/Users/ForgotPassword/Pin", None, json.dumps({"Pin": pin}))
        try:
            result = json.loads(raw)
        except ValueError:
            result = {}
        login = auth_device(base, "fpprobe", pin, "parity-fpprobe")
        r["POST /Users/ForgotPassword/Pin"] = (st == 200 and result.get("Success") is True
                                               and result.get("UsersReset") == ["fpprobe"]
                                               and bool(login.get("AccessToken")))
    else:
        r["POST /Users/ForgotPassword/Pin"] = False   # PIN file unreadable (no docker access?)
    http("DELETE", f"{base}/Users/{uid}", token)   # cleanup
    return r


LIVETV_OPS = [
    "POST /LiveTv/TunerHosts", "POST /LiveTv/ListingProviders",
    "POST /LiveStreams/Open", "GET /LiveTv/LiveStreamFiles/{streamId}/stream.{container}",
    "POST /LiveStreams/Close", "GET /LiveTv/Programs/{programId}", "POST /LiveTv/Timers",
    "GET /LiveTv/Timers/{timerId}", "GET /LiveTv/LiveRecordings/{recordingId}/stream",
    "GET /LiveTv/Recordings/{recordingId}", "DELETE /LiveTv/Timers/{timerId}",
    "DELETE /LiveTv/Recordings/{recordingId}",
]
RECORDING_START_WAIT_S = 60   # the recorder opens the tuner stream + the Recordings folder refreshes
RECORDING_POLL_S = 5
STREAM_PREFIX_BYTES = 16384   # ~87 TS packets: enough for is_mpegts, cheap to pull
STREAM_READ_TIMEOUT_S = 30


def read_prefix(base, path, token, n=STREAM_PREFIX_BYTES, timeout=STREAM_READ_TIMEOUT_S):
    """(status, content-type, first n bytes) of a progressive/endless response — a live tuner
    stream or an in-progress recording never ends, so only a prefix is read."""
    hdr = {"Authorization": f'MediaBrowser Token="{token}", {CLIENT}'}
    req = urllib.request.Request(base + path, headers=hdr)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.headers.get("Content-Type", ""), r.read(n)
    except urllib.error.HTTPError as e:
        return e.code, "", b""
    except (urllib.error.URLError, TimeoutError, ConnectionError, OSError):
        return 0, "", b""


def is_mpegts(body):
    return body[:1] == b"\x47" and len(body) > 188 and body[188:189] == b"\x47"


def mpegts_response(st, ct, body):
    return st == 200 and ct.split(";")[0].strip().lower() == "video/mp2t" and is_mpegts(body)


def j_livetv(base, token, user, _m, _m2):
    """The Live TV flow on the M3U fixture tuner: PlaybackInfo on a channel hands out an
    OpenToken → LiveStreams/Open returns a live media source → its LiveStreamFiles stream
    is a real MPEG-TS → Close revokes it. Then a timer on the programme airing now starts
    a recording → the in-progress recording streams → timer and recording are deleted.
    Every op starts False so an early exit (no channels, no programme, no timer) leaves a
    flagged row, never a missing one."""
    r = dict.fromkeys(LIVETV_OPS, False)
    channels = (get_json(base, f"/LiveTv/Channels?userId={user}", token) or {}).get("Items") or []
    # Provisioning (sweep.provision_livetv) added the tuner host and the listings provider;
    # their effect is what this journey runs on: channels from the tuner, programmes from
    # the guide.
    r["POST /LiveTv/TunerHosts"] = bool(channels)
    if not channels:
        return r
    ch = channels[0]["Id"]
    programs = (get_json(base, f"/LiveTv/Programs?channelIds={ch}&isAiring=true&userId={user}", token)
                or {}).get("Items") or []
    r["POST /LiveTv/ListingProviders"] = bool(programs)
    # --- live stream -------------------------------------------------------------------
    _, raw = http("POST", f"{base}/Items/{ch}/PlaybackInfo?userId={user}", token, json.dumps({}))
    try:
        sources = json.loads(raw).get("MediaSources") or []
    except ValueError:
        sources = []
    token_src = next((s for s in sources if s.get("OpenToken")), None)
    live = {}
    if token_src:
        st, raw = http("POST", f"{base}/LiveStreams/Open", token, json.dumps({
            "OpenToken": token_src["OpenToken"], "UserId": user, "ItemId": ch,
            "PlaySessionId": "parity-livetv", "EnableDirectPlay": True, "EnableDirectStream": True}))
        try:
            live = json.loads(raw).get("MediaSource") or {}
        except ValueError:
            live = {}
        r["POST /LiveStreams/Open"] = st == 200 and bool(live.get("LiveStreamId"))
    stream_id = live.get("LiveStreamId") or ""
    path = live.get("Path") or ""
    if "/LiveTv/LiveStreamFiles/" in path:
        stream_id = path.split("/LiveTv/LiveStreamFiles/", 1)[1].split("/", 1)[0]
    if stream_id:
        r["GET /LiveTv/LiveStreamFiles/{streamId}/stream.{container}"] = mpegts_response(
            *read_prefix(base, f"/LiveTv/LiveStreamFiles/{stream_id}/stream.ts", token))
    if live.get("LiveStreamId"):
        st, _ = http("POST", f"{base}/LiveStreams/Close?liveStreamId={urllib.parse.quote(live['LiveStreamId'])}",
                     token, "")
        gone = read_prefix(base, f"/LiveTv/LiveStreamFiles/{stream_id}/stream.ts", token, timeout=10)[0]
        r["POST /LiveStreams/Close"] = st < 300 and gone != 200
    # --- timer → in-progress recording -------------------------------------------------
    if not programs:
        return r
    prog = programs[0]["Id"]
    got = get_json(base, f"/LiveTv/Programs/{prog}?userId={user}", token) or {}
    r["GET /LiveTv/Programs/{programId}"] = got.get("Id") == prog
    defaults = get_json(base, f"/LiveTv/Timers/Defaults?programId={prog}", token) or {}
    st, _ = http("POST", f"{base}/LiveTv/Timers", token, json.dumps(defaults))
    timers = (get_json(base, f"/LiveTv/Timers?channelId={ch}", token) or {}).get("Items") or []
    timer = next((t for t in timers if t.get("ProgramId") == prog), None) or (timers[0] if timers else None)
    r["POST /LiveTv/Timers"] = st < 300 and timer is not None
    if not timer:
        return r
    tid = timer.get("Id")
    rec = None
    try:
        r["GET /LiveTv/Timers/{timerId}"] = (get_json(base, f"/LiveTv/Timers/{tid}", token) or {}).get("Id") == tid
        for _ in range(RECORDING_START_WAIT_S // RECORDING_POLL_S):
            recs = (get_json(base, f"/LiveTv/Recordings?isInProgress=true&userId={user}", token)
                    or {}).get("Items") or []
            if recs:
                rec = recs[0]
                break
            time.sleep(RECORDING_POLL_S)
        if rec:
            rid = rec["Id"]
            # An in-progress recording is served through /LiveTv/LiveRecordings/{id}/stream,
            # keyed by Jellyfin's INTERNAL timer id (not the timer DTO's hashed id): the only
            # way a client learns it is PlaybackInfo on the recording item, whose media
            # source carries that URL as EncoderPath (the direct Path is the growing file).
            _, raw = http("POST", f"{base}/Items/{rid}/PlaybackInfo?userId={user}", token, json.dumps({}))
            try:
                paths = [(m.get("EncoderPath") or "") + " " + (m.get("Path") or "")
                         for m in json.loads(raw).get("MediaSources") or []]
            except ValueError:
                paths = []
            live_path = next((p for p in paths if "/LiveTv/LiveRecordings/" in p), "")
            if live_path:
                key = live_path.split("/LiveTv/LiveRecordings/", 1)[1].split("/", 1)[0]
                r["GET /LiveTv/LiveRecordings/{recordingId}/stream"] = mpegts_response(
                    *read_prefix(base, f"/LiveTv/LiveRecordings/{key}/stream", token))
            r["GET /LiveTv/Recordings/{recordingId}"] = (
                (get_json(base, f"/LiveTv/Recordings/{rid}?userId={user}", token) or {}).get("Id") == rid)
    finally:
        # Whatever happened above, the timer (and with it the recording in progress) goes.
        st, _ = http("DELETE", f"{base}/LiveTv/Timers/{tid}", token)
        left = (get_json(base, f"/LiveTv/Timers?channelId={ch}", token) or {}).get("Items") or []
        r["DELETE /LiveTv/Timers/{timerId}"] = st < 300 and all(t.get("Id") != tid for t in left)
        if rec:
            st, _ = http("DELETE", f"{base}/LiveTv/Recordings/{rec['Id']}", token)
            still = get_json(base, f"/LiveTv/Recordings/{rec['Id']}?userId={user}", token)
            r["DELETE /LiveTv/Recordings/{recordingId}"] = st < 300 and not still
    return r


def j_remote_subtitles(base, token, user, mid, _m2):
    """Remote subtitles through OpenSubtitles — only when credentials are configured (see
    sweep.opensubtitles_credentials). The fixture's first movie carries a real IMDb id in
    its NFO, so both servers search the same title: search → download the first hit → the
    item gains an external subtitle stream → the provider's own subtitle file is fetched.
    Cost: two provider downloads per server (the POST and the Providers GET both download),
    so four quota units per two-leg run. Jellyfin's plugin uses its own bundled API key,
    Ferrofin uses OPENSUBTITLES_API_KEY — an exhausted quota can therefore flag one side only."""
    if not opensubtitles_credentials():
        return {}
    r = {}
    hits = get_json(base, f"/Items/{mid}/RemoteSearch/Subtitles/eng", token) or []
    r["GET /Items/{itemId}/RemoteSearch/Subtitles/{language}"] = bool(hits)
    sub_id = hits[0].get("Id") if hits else None
    if not sub_id:
        return r
    before = set(external_subtitle_indexes(base, token, user, mid))
    st, _ = http("POST", f"{base}/Items/{mid}/RemoteSearch/Subtitles/{urllib.parse.quote(sub_id, safe='')}",
                 token, "")
    # Jellyfin answers 204 even when the download failed (it logs and swallows), and only
    # QUEUES the refresh that creates the stream row — the read-back below is the verdict.
    # The verdict wants the stream the search asked for: a new EXTERNAL stream tagged eng
    # (the provider id carries the language; a server that drops it lands an "und" stream).
    added = []
    for _ in range(SUBTITLE_REFRESH_WAIT_S):
        added = [i for i, lang in external_subtitles(base, token, user, mid)
                 if i not in before and lang == "eng"]
        if added:
            break
        time.sleep(1)
    r["POST /Items/{itemId}/RemoteSearch/Subtitles/{subtitleId}"] = st < 300 and bool(added)
    st, raw = http("GET", f"{base}/Providers/Subtitles/Subtitles/{urllib.parse.quote(sub_id, safe='')}", token)
    r["GET /Providers/Subtitles/Subtitles/{subtitleId}"] = st == 200 and bool(raw)
    for i, lang in external_subtitles(base, token, user, mid):   # reap, whichever path ran
        # Our own debris whatever language the server gave it, plus any external eng stream
        # (the fixture's own eng track is embedded, never external).
        if i not in before or lang == "eng":
            http("DELETE", f"{base}/Videos/{mid}/Subtitles/{i}", token)
    return r


# ---------------------------------------------------------------- remote search ("Identify")

# The per-result field set compared for every RemoteSearch row. Nothing is excluded:
# a candidate that differs in ANY of these between the two servers fails the row.
IDENTIFY_FIELDS = ("Name", "ProviderIds", "ProductionYear", "PremiereDate", "Overview", "ImageUrl")


def remote_search(base, token, kind, search_info, provider=None):
    """POST /Items/RemoteSearch/{kind}; returns (status, results-list)."""
    body = {"SearchInfo": search_info}
    if provider:
        body["SearchProviderName"] = provider
    st, raw = http("POST", f"{base}/Items/RemoteSearch/{kind}", token, json.dumps(body))
    try:
        out = json.loads(raw) if raw else []
    except ValueError:
        out = []
    return st, (out if isinstance(out, list) else [])


def identify(results, fields=IDENTIFY_FIELDS):
    """Every result projected onto the compared field set, order preserved."""
    return [{k: r.get(k) for k in fields} for r in results]


def j_remote_search_identify(base, token, user, _m, _m2):
    """The Identify flow: POST /Items/RemoteSearch/{Movie,Series,BoxSet,Person,Trailer}.

    These are POSTs by contract but SEARCHES by behaviour — v10.11.8
    `ItemLookupController` only calls `IProviderManager.GetRemoteSearchResults` and returns
    Ok, so nothing is mutated and there is no read-back. They live here because sweep.py is
    GET/HEAD-only and reads.py correlates by Path.

    They hit live remote providers, so the assertions are built to survive drift: both
    servers are queried in the SAME run (so upstream sees one state), and every claim is
    either pinned by ProviderId or is a structural invariant.

    DELIBERATELY NOT COMPARED, and why:
      * Rows from any provider other than the one pinned by `SearchProviderName`. The
        provider SETS differ by owner-accepted design (CLAUDE.md "Current scope"):
        Ferrofin's OMDb is inert without FERROFIN_OMDB_KEY while Jellyfin 10.11.8 ships a
        hardcoded key (MediaBrowser.Providers/Plugins/Omdb/OmdbProvider.cs:257), and
        Ferrofin compiles TheTVDB in where the oracle has no TVDB plugin installed.
        Comparing totals would encode those as a permanent red. Provider identity is still
        asserted: `tmdb_answers` goes false if the TMDB provider stops answering an
        unfiltered search on either side, and every pinned assertion below is scoped to
        TheMovieDb, so answering from the WRONG provider fails the row.
    Everything else IS compared: for every search below the FULL candidate list is projected
    onto IDENTIFY_FIELDS and compared element-for-element, order and count included. That is
    safe despite TMDB's popularity drift only because both servers are queried seconds apart
    in one run; if this row ever flakes, the fix is to keep the comparison and pin the search
    harder (by ProviderId), never to widen it.
      * `POST /Items/RemoteSearch/Trailer` cannot be verified without an OMDb key: OMDb is
        the ONLY provider implementing `IRemoteMetadataProvider<Trailer, TrailerInfo>` in
        v10.11.8, so a keyless Ferrofin can only ever answer []. The row is still probed
        (both statuses, both bogus-term answers, both projections) so the divergence is
        measured rather than assumed; see classifications.json.

    Needs outbound TMDB reachability from both containers; with none, both sides return []
    and the rows fail rather than passing vacuously.
    """
    r = {}
    tmdb = "TheMovieDb"

    # ---- Movie ---------------------------------------------------------------
    # Name+Year: `TmdbMovieProvider` forwards Year to /search/movie and every row carries
    # PremiereDate + ProductionYear from the release date.
    _, named = remote_search(base, token, "Movie",
                             {"Name": "The Matrix", "Year": 1999}, tmdb)
    # A `Tmdb` id short-circuits the name search: exactly one row, with the IMDb id merged.
    _, byid = remote_search(base, token, "Movie", {"ProviderIds": {"Tmdb": "603"}}, tmdb)
    # An `Imdb` id resolves through TMDB's /find.
    _, byimdb = remote_search(base, token, "Movie",
                              {"ProviderIds": {"Imdb": "tt0133093"}}, tmdb)
    _, bogus = remote_search(base, token, "Movie", {"Name": "zzqqxxnotamovie12345"}, tmdb)
    _, unfiltered = remote_search(base, token, "Movie", {"Name": "The Matrix", "Year": 1999})
    ev = {"named": identify(named),
          "byid": identify(byid),
          "byimdb": identify(byimdb),
          "bogus_empty": bogus == [],
          "tmdb_answers": tmdb in {x.get("SearchProviderName") for x in unfiltered}}
    r["POST /Items/RemoteSearch/Movie"] = Same(
        bool(named) and len(byid) == 1 and bool(byimdb)
        and all(x.get("PremiereDate") for x in named)
        and ev["bogus_empty"] and ev["tmdb_answers"], ev)

    # ---- Series --------------------------------------------------------------
    # `TmdbSeriesProvider` leaves SearchSeriesAsync's year at 0, so a WRONG Year must not
    # narrow the search; its mappers set PremiereDate and never ProductionYear; and the
    # by-id branch carries Tmdb + Imdb + Tvdb.
    _, s_named = remote_search(base, token, "Series", {"Name": "Breaking Bad"}, tmdb)
    _, s_year = remote_search(base, token, "Series",
                              {"Name": "Breaking Bad", "Year": 2019}, tmdb)
    _, s_byid = remote_search(base, token, "Series", {"ProviderIds": {"Tmdb": "1396"}}, tmdb)
    _, s_bogus = remote_search(base, token, "Series",
                               {"Name": "zzzqqxnotarealshowxyz123"}, tmdb)
    _, s_unfiltered = remote_search(base, token, "Series", {"Name": "Breaking Bad"})
    ev = {"named": identify(s_named),
          "year_ignored": "1396" in {x.get("ProviderIds", {}).get("Tmdb") for x in s_year},
          "byid": identify(s_byid),
          "bogus_empty": s_bogus == [],
          "tmdb_answers": tmdb in {x.get("SearchProviderName") for x in s_unfiltered}}
    r["POST /Items/RemoteSearch/Series"] = Same(
        bool(s_named) and len(s_byid) == 1 and ev["year_ignored"]
        and all(x.get("PremiereDate") for x in s_named)
        and not any(x.get("ProductionYear") for x in s_named)
        and ev["bogus_empty"] and ev["tmdb_answers"], ev)

    # ---- BoxSet --------------------------------------------------------------
    # `TmdbBoxSetProvider` short-circuits on tmdbId > 0 (one row, no Overview on any search
    # DTO) and otherwise searches /search/collection by name.
    _, b_named = remote_search(base, token, "BoxSet",
                               {"Name": "The Lord of the Rings Collection"}, tmdb)
    _, b_byid = remote_search(base, token, "BoxSet", {"ProviderIds": {"Tmdb": "119"},
                                                      "Name": "The Matrix Collection"}, tmdb)
    _, b_bogus = remote_search(base, token, "BoxSet",
                               {"Name": "zzqxwv nonexistent collection 99812"}, tmdb)
    ev = {"named": identify(b_named),
          "byid": identify(b_byid),
          "bogus_empty": b_bogus == []}
    r["POST /Items/RemoteSearch/BoxSet"] = Same(
        bool(b_named) and len(b_byid) == 1 and ev["bogus_empty"], ev)

    # ---- Person --------------------------------------------------------------
    # Only the by-id branch takes a language upstream (`GetPersonAsync(id, language,
    # countryCode, …)`); `SearchPersonAsync(name, ct)` takes none. `bio_localized` compares
    # each server against ITSELF, so it is drift-proof: it is true only when the fr and en
    # biographies of the SAME person differ.
    _, p_named = remote_search(base, token, "Person", {"Name": "Tom Hanks"}, tmdb)
    _, p_fr = remote_search(base, token, "Person",
                            {"ProviderIds": {"Tmdb": "31"}, "MetadataLanguage": "fr",
                             "MetadataCountryCode": "FR"}, tmdb)
    _, p_en = remote_search(base, token, "Person",
                            {"ProviderIds": {"Tmdb": "31"}, "MetadataLanguage": "en",
                             "MetadataCountryCode": "US"}, tmdb)
    _, p_bogus = remote_search(base, token, "Person",
                               {"Name": "zzqqxxnotarealpersonzzz"}, tmdb)
    _, p_noprov = remote_search(base, token, "Person", {"Name": "Tom Hanks"}, "NoSuchProvider")
    fr_bio = p_fr[0].get("Overview") if p_fr else None
    en_bio = p_en[0].get("Overview") if p_en else None
    ev = {"named": identify(p_named),
          "byid_ids": [x.get("ProviderIds") for x in p_en],
          "byid_name": [x.get("Name") for x in p_en],
          "bio_localized": bool(fr_bio) and bool(en_bio) and fr_bio != en_bio,
          "bogus_empty": p_bogus == [],
          "provider_filter_empty": p_noprov == []}
    r["POST /Items/RemoteSearch/Person"] = Same(
        bool(p_named) and len(p_en) == 1 and ev["bio_localized"]
        and ev["bogus_empty"] and ev["provider_filter_empty"], ev)

    # ---- Trailer -------------------------------------------------------------
    # OMDb is the only Trailer fetcher on either side, so this row measures the OMDb-key
    # divergence rather than hiding it.
    omdb = "The Open Movie Database"
    t_st, t_named = remote_search(base, token, "Trailer",
                                  {"Name": "Inception", "Year": 2010}, omdb)
    _, t_bogus = remote_search(base, token, "Trailer",
                               {"Name": "zzqxwvyunlikelytitle12345"}, omdb)
    ev = {"status": t_st,
          "named": identify(t_named),
          "bogus_empty": t_bogus == []}
    r["POST /Items/RemoteSearch/Trailer"] = Same(
        t_st == 200 and bool(t_named) and ev["bogus_empty"], ev)
    return r


def j_backup(base, token, user, _m, _m2):
    """Backup create → manifest → list on the server's own data dir. The manifest must echo
    the posted options, the Manifest route must read the same manifest back by the returned
    path, and the listing must contain it. (Restore restarts the server: terminal.py.)"""
    r = {}
    opts = {"Metadata": False, "Trickplay": False, "Subtitles": False, "Database": True}
    st, raw = http("POST", f"{base}/Backup/Create", token, json.dumps(opts))
    try:
        created = json.loads(raw)
    except ValueError:
        created = {}
    path = created.get("Path") or ""
    r["POST /Backup/Create"] = (st == 200 and bool(path) and created.get("Options") == opts
                                and bool(created.get("BackupEngineVersion"))
                                and bool(created.get("DateCreated")))
    if path:
        manifest = get_json(base, "/Backup/Manifest?path=" + urllib.parse.quote(path), token) or {}
        r["GET /Backup/Manifest"] = manifest == created
        listed = get_json(base, "/Backup", token) or []
        r["GET /Backup"] = any(m.get("Path") == path for m in listed)
    return r


JOURNEYS = [j_startup,   # first: see its docstring
            j_favorites, j_played, j_rating, j_playlist, j_collection, j_users, j_item_edit,
            j_api_keys, j_user_item_data, j_display_prefs, j_scheduled_task_triggers,
            j_device_options, j_playstate, j_capabilities, j_user_config, j_system_config,
            j_playlist_share, j_item_delete, j_capabilities_query, j_environment_validate,
            j_merge_versions, j_playing_items, j_virtualfolder_rename,
            j_users_password, j_virtualfolder_crud, j_sessions, j_config_writes,
            j_scheduled_run, j_playbackinfo_post, j_active_encodings, j_clientlog,
            j_authenticate, j_user_update, j_devices_delete, j_bulk_item_delete,
            j_subtitles_upload, j_quickconnect, j_system_and_refresh,
            j_forgot_password, j_backup, j_livetv, j_remote_subtitles,
            j_remote_search_identify,
            # Destructive: merges/splits the shared movies, so it must run LAST so its
            # mutations can't corrupt the items other journeys read/refresh.
            j_merge_versions_controller]

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


def journeys(ferrofin_url, jellyfin_url):
    ht, hu = bring_up(ferrofin_url, "ferrofin")
    h = run_all(ferrofin_url, ht, hu)
    j = {}
    if jellyfin_url:
        jt, ju = bring_up(jellyfin_url, "jellyfin")
        j = run_all(jellyfin_url, jt, ju)

    rows = {}
    for op in sorted(k for k in h if not k.startswith("_")):
        h_ok = h.get(op)
        j_ok = j.get(op)
        if jellyfin_url:
            # A step that returned `Same` also has to end in the same state on both
            # servers, not merely round-trip its own write on each.
            cross = cross_server_ok(h_ok, j_ok)
            deep = bool(h_ok and j_ok and cross)
            note = f"H={h_ok} J={j_ok}"
            if h_ok and not j_ok:
                cls = "flagged: Jellyfin read-back differed (verify: oracle setup or Ferrofin extra)"
            elif not h_ok and j_ok:
                cls = "flagged: Ferrofin read-back did not reflect the write (verify: real gap vs read-back method)"
            elif not h_ok:
                cls = "flagged: write effect not observed on either server (likely corpus/setup)"
            elif not cross:
                cls = ("flagged: both servers round-tripped their own write, but ended in "
                       "different states (verify: a default divergence, not a write gap)")
                note += f" [{evidence_diff(h_ok.evidence, j_ok.evidence)}]"
            else:
                cls = "ok"
            rows[op] = {"deep_verified": deep, "classification": cls, "note": note}
        else:
            rows[op] = {"deep_verified": bool(h_ok), "classification": "ok" if h_ok else "write effect not confirmed on Ferrofin",
                        "note": f"H={h_ok}"}
    return rows, {k: v for k, v in h.items() if k.startswith("_")}


def main():
    if "--check" in sys.argv:
        selfcheck()
        return
    ferrofin = os.environ.get("FERROFIN_URL", "http://localhost:18096")
    jellyfin = os.environ.get("JELLYFIN_URL")
    rows, errors = journeys(ferrofin, jellyfin)
    out = {"generated_by": "suite/parity/journeys.py", "last_verified": os.environ.get("PARITY_STAMP", ""),
           "errors": errors, "rows": rows}
    with open(os.path.join(ROOT, "suite/parity/journey-results.json"), "w") as f:
        json.dump(out, f, indent=2, sort_keys=True)
        f.write("\n")
    ok = sum(1 for v in rows.values() if v["deep_verified"])
    print(f"wrote parity/journey-results.json — {len(rows)} write ops, {ok} deep-verified"
          + (f", errors: {list(errors)}" if errors else ""))


def selfcheck():
    # The combine logic: deep_verified only when the effect holds on BOTH servers, and —
    # for a `Same` step — only when both servers ended in the same state.
    def combine(h_ok, j_ok):
        return bool(h_ok and j_ok and cross_server_ok(h_ok, j_ok))
    assert combine(True, True) is True
    assert combine(True, False) is False   # Jellyfin disagrees → not verified
    assert combine(False, True) is False   # real Ferrofin gap → not verified
    # Two servers that each faithfully round-trip DIFFERENT defaults are not parity.
    assert combine(Same(True, {"EnableRealtimeMonitor": True}),
                   Same(True, {"EnableRealtimeMonitor": False})) is False
    assert combine(Same(True, {"EnableRealtimeMonitor": False}),
                   Same(True, {"EnableRealtimeMonitor": False})) is True
    assert combine(Same(False, {"a": 1}), Same(True, {"a": 1})) is False
    assert evidence_diff({"a": 1, "b": 2}, {"a": 1, "b": 3}) == "b: H=2 J=3"
    assert evidence_diff({"a": 1}, {"a": 1}) == "(no key differs)"
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
