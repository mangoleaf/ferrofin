#!/usr/bin/env python3
"""End-to-end probe: casting (remote control) and SyncPlay over real sockets.

Usage:
    FERROFIN_BASE=http://127.0.0.1:8096 \
    FERROFIN_USER=admin FERROFIN_PASS=... \
    python3 suite/ws/probe_remote_control.py

Two authenticated sessions (a controller and a target), each with a live
/socket, then every remote-control and SyncPlay verb, asserting on what the
*receiving socket* actually gets. Prints a PASS/FAIL table and exits non-zero
if anything expected never arrived.
"""

import os
import sys
import time
import uuid

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from wsclient import WS, http  # noqa: E402

USER = os.environ.get("FERROFIN_USER", "admin")
PASS = os.environ.get("FERROFIN_PASS", "")

RESULTS = []


def check(name, ok, detail=""):
    RESULTS.append((name, bool(ok), detail))
    print(f"{'PASS' if ok else 'FAIL'}  {name}" + (f"  — {detail}" if detail else ""))
    return ok


def login(device_id, client="Probe", device="Probe"):
    ident = dict(client=client, device=device, device_id=device_id, version="1")
    status, body = http(
        "POST", "/Users/AuthenticateByName",
        body={"Username": USER, "Pw": PASS}, **ident,
    )
    if status != 200:
        raise SystemExit(f"login failed for {device_id}: {status} {body!r}")
    return {
        "token": body["AccessToken"],
        "session_id": body["SessionInfo"]["Id"],
        "user_id": body["User"]["Id"],
        "ident": ident,
    }


def socket_for(sess):
    return WS(f"/socket?api_key={sess['token']}&deviceId={sess['ident']['device_id']}")


def main():
    controller = login("probe-controller", client="ProbeController", device="Controller")
    target = login("probe-target", client="ProbeTarget", device="Living Room TV")
    print(f"controller session {controller['session_id']}  target session {target['session_id']}")

    ws_c = socket_for(controller)
    ws_t = socket_for(target)
    time.sleep(0.4)

    # ---- the target advertises itself as remote-controllable ---------------
    status, _ = http(
        "POST", "/Sessions/Capabilities/Full", token=target["token"],
        body={
            "PlayableMediaTypes": ["Video", "Audio"],
            "SupportedCommands": ["Play", "PlayState", "DisplayMessage", "SetVolume", "GoHome", "DisplayContent"],
            "SupportsMediaControl": True,
            "SupportsPersistentIdentifier": True,
        },
        **target["ident"],
    )
    check("POST /Sessions/Capabilities/Full accepted", status in (200, 204), f"status {status}")

    # ---- the controller can see it ----------------------------------------
    status, sessions = http(
        "GET", f"/Sessions?ControllableByUserId={controller['user_id']}",
        token=controller["token"], **controller["ident"],
    )
    listed = [s for s in (sessions or []) if s.get("Id") == target["session_id"]] if status == 200 else []
    check("target appears in /Sessions?ControllableByUserId", bool(listed), f"status {status}")
    if listed:
        t = listed[0]
        check("  SupportsRemoteControl true", t.get("SupportsRemoteControl") is True, str(t.get("SupportsRemoteControl")))
        check("  Capabilities.SupportedCommands populated",
              bool((t.get("Capabilities") or {}).get("SupportedCommands")),
              str((t.get("Capabilities") or {}).get("SupportedCommands")))
        check("  PlayableMediaTypes populated", bool(t.get("PlayableMediaTypes")), str(t.get("PlayableMediaTypes")))

    # ---- pick a real item to cast -----------------------------------------
    status, items = http(
        "GET", f"/Items?UserId={controller['user_id']}&Recursive=true&Limit=1&IncludeItemTypes=Movie,Episode,Audio",
        token=controller["token"], **controller["ident"],
    )
    item_id = None
    folder_id = None
    real_item = True
    if status == 200 and (items or {}).get("Items"):
        item_id = items["Items"][0]["Id"]
    if item_id is None:
        item_id, real_item = uuid.uuid4().hex, False
        print("  (no library items — casting a synthetic id; delivery is still proven)")
    status, folders = http(
        "GET", f"/Items?UserId={controller['user_id']}&Recursive=true&Limit=1&IncludeItemTypes=Series,MusicAlbum",
        token=controller["token"], **controller["ident"],
    )
    if status == 200 and (folders or {}).get("Items"):
        folder_id = folders["Items"][0]["Id"]

    # ---- cast: Play --------------------------------------------------------
    ws_t.drain()
    status, _ = http(
        "POST", f"/Sessions/{target['session_id']}/Playing?playCommand=PlayNow&itemIds={item_id}",
        token=controller["token"], **controller["ident"],
    )
    msg = ws_t.wait("Play")
    check("POST /Sessions/{id}/Playing -> target receives Play", msg is not None, f"http {status}, got {ws_t.types()}")
    if msg:
        data = msg.get("Data") or {}
        got_ids = [str(i).replace("-", "").lower() for i in (data.get("ItemIds") or [])]
        if real_item:
            check("  Play.ItemIds carries the cast item",
                  got_ids == [item_id.replace("-", "").lower()], str(data.get("ItemIds")))
        else:
            # Translation resolves ids against the library; an id that is not
            # there contributes nothing (C# logs and drops it).
            check("  Play.ItemIds drops an id with no library item",
                  got_ids == [], str(data.get("ItemIds")))
        check("  Play.ControllingUserId set (Jellyfin sets it)",
              data.get("ControllingUserId") not in (None, "", "00000000-0000-0000-0000-000000000000"),
              str(data.get("ControllingUserId")))

    # ---- cast: folder expansion (Jellyfin TranslateItemForPlayback) --------
    if folder_id:
        ws_t.drain()
        http("POST", f"/Sessions/{target['session_id']}/Playing?playCommand=PlayNow&itemIds={folder_id}",
             token=controller["token"], **controller["ident"])
        msg = ws_t.wait("Play")
        ids = ((msg or {}).get("Data") or {}).get("ItemIds") or []
        check("casting a Series/Album expands to children (server-side)",
              len(ids) > 1 or (len(ids) == 1 and ids[0] != folder_id),
              f"sent 1 folder id, target got {len(ids)}: {ids[:3]}")

    # ---- cast: PlayShuffle / PlayInstantMix translation --------------------
    for cmd in ("PlayShuffle", "PlayInstantMix"):
        ws_t.drain()
        http("POST", f"/Sessions/{target['session_id']}/Playing?playCommand={cmd}&itemIds={item_id}",
             token=controller["token"], **controller["ident"])
        msg = ws_t.wait("Play")
        got = ((msg or {}).get("Data") or {}).get("PlayCommand")
        check(f"{cmd} translated to PlayNow before push", got == "PlayNow", f"target saw PlayCommand={got}")

    # ---- cast: Playstate ---------------------------------------------------
    for verb, expect in (("Pause", "Pause"), ("Unpause", "Unpause"), ("Stop", "Stop")):
        ws_t.drain()
        status, _ = http("POST", f"/Sessions/{target['session_id']}/Playing/{verb}",
                         token=controller["token"], **controller["ident"])
        msg = ws_t.wait("Playstate")
        got = ((msg or {}).get("Data") or {}).get("Command")
        check(f"Playstate {verb} reaches target", got == expect, f"http {status}, got {got}")

    ws_t.drain()
    status, _ = http("POST", f"/Sessions/{target['session_id']}/Playing/Seek?seekPositionTicks=1200000000",
                     token=controller["token"], **controller["ident"])
    msg = ws_t.wait("Playstate")
    data = (msg or {}).get("Data") or {}
    check("Playstate Seek carries SeekPositionTicks",
          data.get("Command") == "Seek" and data.get("SeekPositionTicks") == 1200000000, str(data))

    # ---- cast: GeneralCommand ---------------------------------------------
    ws_t.drain()
    status, _ = http("POST", f"/Sessions/{target['session_id']}/Command/SetVolume?arguments[Volume]=42",
                     token=controller["token"], **controller["ident"])
    msg = ws_t.wait("GeneralCommand")
    check("GeneralCommand reaches target", msg is not None, f"http {status}, got {ws_t.types()}")

    ws_t.drain()
    status, _ = http("POST", f"/Sessions/{target['session_id']}/Message",
                     token=controller["token"],
                     body={"Text": "hello", "Header": "probe", "TimeoutMs": 5000},
                     **controller["ident"])
    msg = ws_t.wait("GeneralCommand") or ws_t.wait("MessageCommand", timeout=1.0)
    check("DisplayMessage reaches target", msg is not None, f"http {status}, got {ws_t.types()}")

    ws_t.drain()
    status, _ = http("POST", f"/Sessions/{target['session_id']}/System/GoHome",
                     token=controller["token"], **controller["ident"])
    msg = ws_t.wait("GeneralCommand")
    check("System command reaches target", msg is not None, f"http {status}, got {ws_t.types()}")

    ws_t.drain()
    status, _ = http(
        "POST",
        f"/Sessions/{target['session_id']}/Viewing?itemType=Movie&itemId={item_id}&itemName=Probe",
        token=controller["token"], **controller["ident"],
    )
    msg = ws_t.wait("GeneralCommand")
    check("DisplayContent (browse-to) reaches target", msg is not None, f"http {status}, got {ws_t.types()}")

    # ---- SyncPlay ----------------------------------------------------------
    print("\n--- SyncPlay ---")
    ws_c.drain(); ws_t.drain()
    status, group = http("POST", "/SyncPlay/New", token=controller["token"],
                         body={"GroupName": "probe-group"}, **controller["ident"])
    check("POST /SyncPlay/New returns a group", status == 200 and isinstance(group, dict) and group.get("GroupId"),
          f"status {status} {group!r}")
    msg = ws_c.wait("SyncPlayGroupUpdate")
    check("creator receives GroupJoined push",
          ((msg or {}).get("Data") or {}).get("Type") == "GroupJoined", str((msg or {}).get("Data")))
    group_id = (group or {}).get("GroupId")

    status, listed = http("GET", "/SyncPlay/List", token=target["token"], **target["ident"])
    check("GET /SyncPlay/List shows the group",
          status == 200 and any(g.get("GroupId") == group_id for g in (listed or [])), f"status {status}")

    ws_c.drain(); ws_t.drain()
    status, _ = http("POST", "/SyncPlay/Join", token=target["token"],
                     body={"GroupId": group_id}, **target["ident"])
    joined = ws_t.wait("SyncPlayGroupUpdate", predicate=lambda m: (m.get("Data") or {}).get("Type") == "GroupJoined")
    check("joiner receives GroupJoined push", joined is not None, f"http {status}, got {ws_t.types()}")
    user_joined = ws_c.wait("SyncPlayGroupUpdate", predicate=lambda m: (m.get("Data") or {}).get("Type") == "UserJoined")
    check("existing member receives UserJoined push", user_joined is not None, f"got {ws_c.types()}")

    ws_c.drain(); ws_t.drain()
    status, _ = http("POST", "/SyncPlay/SetNewQueue", token=controller["token"],
                     body={"PlayingQueue": [item_id], "PlayingItemPosition": 0, "StartPositionTicks": 0},
                     **controller["ident"])
    pq_c = ws_c.wait("SyncPlayGroupUpdate", predicate=lambda m: (m.get("Data") or {}).get("Type") == "PlayQueue")
    pq_t = ws_t.wait("SyncPlayGroupUpdate", predicate=lambda m: (m.get("Data") or {}).get("Type") == "PlayQueue")
    check("SetNewQueue -> PlayQueue push to both members", pq_c is not None and pq_t is not None,
          f"http {status}, controller={pq_c is not None} target={pq_t is not None}")
    cmd_t = ws_t.wait("SyncPlayCommand")
    data = (cmd_t or {}).get("Data") or {}
    check("SetNewQueue -> Unpause SyncPlayCommand with a future When",
          data.get("Command") == "Unpause" and data.get("When"), str(data)[:200])

    for verb, expect in (("Pause", "Pause"), ("Unpause", "Unpause"), ("Stop", "Stop")):
        ws_c.drain(); ws_t.drain()
        status, _ = http("POST", f"/SyncPlay/{verb}", token=controller["token"], **controller["ident"])
        got_c = ws_c.wait("SyncPlayCommand")
        got_t = ws_t.wait("SyncPlayCommand")
        cc = ((got_c or {}).get("Data") or {}).get("Command")
        ct = ((got_t or {}).get("Data") or {}).get("Command")
        check(f"SyncPlay {verb} broadcasts to every member", cc == expect and ct == expect,
              f"http {status}, controller={cc} target={ct}")

    ws_c.drain(); ws_t.drain()
    http("POST", "/SyncPlay/Unpause", token=controller["token"], **controller["ident"])
    ws_c.drain(); ws_t.drain()
    status, _ = http("POST", "/SyncPlay/Seek", token=target["token"],
                     body={"PositionTicks": 600000000}, **target["ident"])
    seek = ws_c.wait("SyncPlayCommand")
    check("a non-owner member can seek the group",
          ((seek or {}).get("Data") or {}).get("Command") in ("Seek", "Pause"), f"http {status}, got {ws_c.types()}")

    ws_c.drain(); ws_t.drain()
    status, _ = http("POST", "/SyncPlay/Leave", token=target["token"], **target["ident"])
    left = ws_t.wait("SyncPlayGroupUpdate", predicate=lambda m: (m.get("Data") or {}).get("Type") == "GroupLeft")
    user_left = ws_c.wait("SyncPlayGroupUpdate", predicate=lambda m: (m.get("Data") or {}).get("Type") == "UserLeft")
    check("Leave -> GroupLeft to leaver", left is not None, f"http {status}, got {ws_t.types()}")
    check("Leave -> UserLeft to remaining member", user_left is not None, f"got {ws_c.types()}")

    # ---- SyncPlay access policy -------------------------------------------
    print("\n--- SyncPlay access policy ---")
    status, _ = http("POST", "/SyncPlay/Pause", token=target["token"], **target["ident"])
    check("playback verb from a non-member is rejected (Jellyfin: 403)", status == 403,
          f"status {status} (Ferrofin returns 204 + a NotInGroup push)")

    ws_c.close(); ws_t.close()

    failed = [n for n, ok, _ in RESULTS if not ok]
    print(f"\n{len(RESULTS) - len(failed)}/{len(RESULTS)} checks passed")
    if failed:
        print("failed:")
        for n in failed:
            print(f"  - {n}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
