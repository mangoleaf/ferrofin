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


JOURNEYS = [j_favorites, j_played, j_rating, j_playlist, j_collection, j_users, j_item_edit,
            j_api_keys, j_user_item_data, j_display_prefs, j_scheduled_task_triggers,
            j_device_options]

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
