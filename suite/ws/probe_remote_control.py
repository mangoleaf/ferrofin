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


SKIPPED = []


def check(name, ok, detail=""):
    RESULTS.append((name, bool(ok), detail))
    print(f"{'PASS' if ok else 'FAIL'}  {name}" + (f"  — {detail}" if detail else ""))
    return ok


def skip(name, why):
    """Records a check that could not run — never counted as a pass."""
    SKIPPED.append(name)
    print(f"SKIP  {name}  — {why}")


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
    # A Movie, specifically: casting a lone Episode legitimately expands to the
    # rest of its series when the user auto-plays next episodes, which would make
    # the "carries exactly the cast item" check ambiguous.
    status, items = http(
        "GET", f"/Items?UserId={controller['user_id']}&Recursive=true&Limit=1&IncludeItemTypes=Movie",
        token=controller["token"], **controller["ident"],
    )
    if status != 200 or not (items or {}).get("Items"):
        status, items = http(
            "GET", f"/Items?UserId={controller['user_id']}&Recursive=true&Limit=1&IncludeItemTypes=Audio",
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

    # ---- the return path: what the target plays shows up on the controller --
    # Without this a cast "works" but the remote-control UI stays blank.
    if real_item:
        http("POST", "/Sessions/Playing", token=target["token"],
             body={"ItemId": item_id, "PositionTicks": 0, "IsPaused": False,
                   "PlayMethod": "DirectPlay"},
             **target["ident"])
        http("POST", "/Sessions/Playing/Progress", token=target["token"],
             body={"ItemId": item_id, "PositionTicks": 900000000, "IsPaused": True,
                   "PlayMethod": "DirectPlay"},
             **target["ident"])
        status, sessions = http(
            "GET", f"/Sessions?ControllableByUserId={controller['user_id']}",
            token=controller["token"], **controller["ident"],
        )
        seen = next((s for s in (sessions or []) if s.get("Id") == target["session_id"]), None)
        now_playing = (seen or {}).get("NowPlayingItem") or {}
        play_state = (seen or {}).get("PlayState") or {}
        check("the target's NowPlayingItem is visible to the controller",
              str(now_playing.get("Id", "")).replace("-", "").lower()
              == item_id.replace("-", "").lower(),
              f"http {status}, got {now_playing.get('Id')}")
        check("the target's PlayState (position + paused) is visible to the controller",
              play_state.get("PositionTicks") == 900000000 and play_state.get("IsPaused") is True,
              str(play_state))
        http("POST", "/Sessions/Playing/Stopped", token=target["token"],
             body={"ItemId": item_id, "PositionTicks": 900000000}, **target["ident"])
    else:
        skip("NowPlayingItem / PlayState round-trip", "server has no library item")

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

    # A queue change is refused unless every member can see the items, so these
    # need a real library item — an idle group ignores the transport verbs.
    if real_item:
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
              ((seek or {}).get("Data") or {}).get("Command") in ("Seek", "Pause"),
              f"http {status}, got {ws_c.types()}")

        # ---- the queue-editing and per-member verbs ------------------------
        # Each must be accepted and reach the other member, so the whole
        # /SyncPlay surface is push-verified rather than status-verified.
        def playlist_item_ids():
            """The group's current queue, as server-assigned PlaylistItemIds."""
            ws_c.drain()
            http("POST", "/SyncPlay/Ping", token=controller["token"],
                 body={"Ping": 1}, **controller["ident"])
            st, g = http("GET", f"/SyncPlay/{group_id}", token=controller["token"],
                         **controller["ident"])
            return st, g

        ws_c.drain(); ws_t.drain()
        status, _ = http("POST", "/SyncPlay/Queue", token=controller["token"],
                         body={"ItemIds": [item_id], "Mode": "Queue"}, **controller["ident"])
        pq = ws_t.wait("SyncPlayGroupUpdate",
                       predicate=lambda m: (m.get("Data") or {}).get("Type") == "PlayQueue")
        queued = ((pq or {}).get("Data") or {}).get("Data") or {}
        playlist = queued.get("Playlist") or []
        check("Queue appends to the group queue and pushes the new PlayQueue",
              status == 204 and len(playlist) >= 2, f"http {status}, playlist={len(playlist)}")

        ids = [p.get("PlaylistItemId") for p in playlist]
        if len(ids) >= 2:
            for name, uri, body, want_len in (
                ("SetPlaylistItem", "/SyncPlay/SetPlaylistItem", {"PlaylistItemId": ids[1]}, None),
                ("MovePlaylistItem", "/SyncPlay/MovePlaylistItem",
                 {"PlaylistItemId": ids[1], "NewIndex": 0}, None),
                ("RemoveFromPlaylist", "/SyncPlay/RemoveFromPlaylist",
                 {"PlaylistItemIds": [ids[0]]}, len(ids) - 1),
            ):
                ws_c.drain(); ws_t.drain()
                status, _ = http("POST", uri, token=controller["token"], body=body,
                                 **controller["ident"])
                got = ws_t.wait("SyncPlayGroupUpdate",
                                predicate=lambda m: (m.get("Data") or {}).get("Type") == "PlayQueue")
                detail = f"http {status}, got {ws_t.types()}"
                if want_len is None:
                    check(f"{name} pushes an updated PlayQueue", status == 204 and got is not None,
                          detail)
                else:
                    now = (((got or {}).get("Data") or {}).get("Data") or {}).get("Playlist") or []
                    check(f"{name} pushes an updated PlayQueue",
                          status == 204 and len(now) == want_len,
                          f"{detail}, playlist={len(now)}")
        else:
            skip("SetPlaylistItem / MovePlaylistItem / RemoveFromPlaylist",
                 "queue too short to edit")

        # Next/Previous only move (and so only broadcast) on a queue with
        # somewhere to go — re-seed a two-entry queue before exercising them.
        ws_c.drain(); ws_t.drain()
        http("POST", "/SyncPlay/SetNewQueue", token=controller["token"],
             body={"PlayingQueue": [item_id, item_id], "PlayingItemPosition": 0,
                   "StartPositionTicks": 0},
             **controller["ident"])
        pq = ws_t.wait("SyncPlayGroupUpdate",
                       predicate=lambda m: (m.get("Data") or {}).get("Type") == "PlayQueue")
        ids = [p.get("PlaylistItemId")
               for p in ((((pq or {}).get("Data") or {}).get("Data") or {}).get("Playlist") or [])]

        for name, uri, body in (
            # C# guards these on the *current* item, so a stale client cannot
            # skip twice; NextItem moves 0→1, PreviousItem moves back.
            ("NextItem", "/SyncPlay/NextItem", {"PlaylistItemId": ids[0] if ids else None}),
            ("PreviousItem", "/SyncPlay/PreviousItem", {"PlaylistItemId": ids[1] if len(ids) > 1 else None}),
            ("SetRepeatMode", "/SyncPlay/SetRepeatMode", {"Mode": "RepeatAll"}),
            ("SetShuffleMode", "/SyncPlay/SetShuffleMode", {"Mode": "Shuffle"}),
        ):
            ws_c.drain(); ws_t.drain()
            status, _ = http("POST", uri, token=controller["token"], body=body,
                             **controller["ident"])
            time.sleep(0.4)
            check(f"{name} is accepted and reaches the other member",
                  status == 204 and bool(ws_t.types()), f"http {status}, got {ws_t.types()}")

        # Per-member verbs mutate the caller's own member state; they are
        # accepted and must not disturb the group (C# handles them per session).
        for name, uri, body in (
            ("Buffering", "/SyncPlay/Buffering",
             {"When": "2026-01-01T00:00:00Z", "PositionTicks": 0, "IsPlaying": False,
              "PlaylistItemId": ids[0] if ids else str(uuid.uuid4())}),
            ("Ready", "/SyncPlay/Ready",
             {"When": "2026-01-01T00:00:00Z", "PositionTicks": 0, "IsPlaying": False,
              "PlaylistItemId": ids[0] if ids else str(uuid.uuid4())}),
            ("SetIgnoreWait", "/SyncPlay/SetIgnoreWait", {"IgnoreWait": True}),
            ("Ping", "/SyncPlay/Ping", {"Ping": 42}),
        ):
            status, _ = http("POST", uri, token=target["token"], body=body, **target["ident"])
            check(f"{name} is accepted from a member", status == 204, f"http {status}")

        st, _ = playlist_item_ids()
        check("the group survives the whole verb sweep", st == 200, f"GET /SyncPlay/{{id}} -> {st}")
    else:
        skip("SyncPlay queue + transport verbs",
             "server has no library item; a queue of unresolvable ids is refused by design")

    # A queue nobody can resolve must be refused outright, with no broadcast.
    time.sleep(0.5)  # let the previous step's pushes land before draining
    ws_c.drain(); ws_t.drain()
    status, _ = http("POST", "/SyncPlay/SetNewQueue", token=controller["token"],
                     body={"PlayingQueue": [str(uuid.uuid4())], "PlayingItemPosition": 0,
                           "StartPositionTicks": 0},
                     **controller["ident"])
    time.sleep(0.5)
    check("a queue of items no member can see is refused silently",
          status == 204 and not ws_c.types() and not ws_t.types(),
          f"http {status}, controller={ws_c.types()} target={ws_t.types()}")

    ws_c.drain(); ws_t.drain()
    status, _ = http("POST", "/SyncPlay/Leave", token=target["token"], **target["ident"])
    left = ws_t.wait("SyncPlayGroupUpdate", predicate=lambda m: (m.get("Data") or {}).get("Type") == "GroupLeft")
    user_left = ws_c.wait("SyncPlayGroupUpdate", predicate=lambda m: (m.get("Data") or {}).get("Type") == "UserLeft")
    check("Leave -> GroupLeft to leaver", left is not None, f"http {status}, got {ws_t.types()}")
    check("Leave -> UserLeft to remaining member", user_left is not None, f"got {ws_c.types()}")

    # ---- SyncPlay access policy -------------------------------------------
    # `IsInGroup` is a per-*user* check (C# `ISyncPlayManager.IsUserActive`), and
    # both probe sessions are the same user — so the controller has to leave too
    # before the user counts as out of every group.
    print("\n--- SyncPlay access policy ---")
    http("POST", "/SyncPlay/Leave", token=controller["token"], **controller["ident"])
    status, _ = http("POST", "/SyncPlay/Pause", token=target["token"], **target["ident"])
    check("playback verb from a non-member is rejected (Jellyfin: 403)", status == 403,
          f"status {status}")
    status, _ = http("POST", "/SyncPlay/Leave", token=target["token"], **target["ident"])
    check("Leave from a non-member is rejected (Jellyfin: 403)", status == 403, f"status {status}")

    # Creating a group is allowed again once the policy permits it, and the
    # group verbs unlock with membership.
    status, group = http("POST", "/SyncPlay/New", token=controller["token"],
                         body={"GroupName": "policy-check"}, **controller["ident"])
    check("New is allowed for a CreateAndJoinGroups user", status == 200, f"status {status}")
    status, _ = http("POST", "/SyncPlay/Pause", token=controller["token"], **controller["ident"])
    check("playback verb is allowed once in a group", status == 204, f"status {status}")
    http("POST", "/SyncPlay/Leave", token=controller["token"], **controller["ident"])

    ws_c.close(); ws_t.close()

    failed = [n for n, ok, _ in RESULTS if not ok]
    print(f"\n{len(RESULTS) - len(failed)}/{len(RESULTS)} checks passed"
          + (f", {len(SKIPPED)} skipped" if SKIPPED else ""))
    for name in SKIPPED:
        print(f"  skipped: {name}")
    if failed:
        print("failed:")
        for n in failed:
            print(f"  - {n}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
