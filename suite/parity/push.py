#!/usr/bin/env python3
"""Layer-2 PUSH differential: the server→client WebSocket messages an op causes.

Some operations say almost nothing in their HTTP response — `POST /SyncPlay/Join`
answers `204` with a zero-byte body — and everything they actually DO arrives
later, over the client's `/socket`, as a `SyncPlayGroupUpdate` or a
`SyncPlayCommand`. No HTTP-body layer can see that, so 21 SyncPlay ops sat in the
ledger wearing a curated sentence ("the response body carries no signal here")
that was never earned by a measurement, and the sentence hid a real port gap:
Jellyfin pushed TWO messages where Ferrofin pushed one.

This layer opens real sockets on BOTH servers, issues the same op against each,
collects what each server pushed, and compares them:

  * the ordered SEQUENCE of messages per receiving socket — which types arrived
    and how many. "Jellyfin sent two and Ferrofin sent one" is the finding this
    layer exists to produce, and an absent message is reported as absent, never
    quietly folded into agreement;
  * every non-volatile field of each matched message's payload, through the same
    `parity_diff` the read layer uses;
  * plus, where the op returns a body, that body — diffed the same way.

That is a genuine two-server differential, so it earns a method name of its own:
`push-diff`, the SIXTH member of the closed set in `parity/verification.py`. It is
deliberately NOT `body-diff`: no HTTP response body is what carries the claim, so
it must not be counted in the ledger's headline. A row is stamped `push-diff` only
when the message sets AND the body (if any) all diff clean — the stamp can never
be weaker than the thing it sits beside.

`suite/ws/probe_remote_control.py` remains the Ferrofin-only smoke test (it drives
far more verbs, but asserts only Ferrofin's own shape and writes no results file).
THIS is the differential the ledger ingests.

Run against a provisioned pair:
  FERROFIN_URL=... JELLYFIN_URL=... parity/push.py
Offline self-test (proves the differ REJECTS):
  parity/push.py --check
"""
import json
import os
import re
import sys
import time
import uuid

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.join(ROOT, "suite/ws"))
import parity_diff      # noqa: E402 — the same deep-diff the read layer walks
import verification     # noqa: E402 — the closed set of verification methods
from wsclient import WS, http as ws_http   # noqa: E402

USER = os.environ.get("BENCH_ADMIN_USER", "bench")
PASS = os.environ.get("BENCH_ADMIN_PASSWORD", "benchpass123")

#: Keys that cannot match between two independent instances, ON TOP of the global
#: `parity_diff.VOLATILE`. Scoped to this layer on purpose — widening the global
#: denylist to make a push row green would blind every other layer too.
#:
#:   MessageId      every outbound envelope carries a fresh GUID
#:                  (`OutboundWebSocketMessage` / `envelope()`), minted per message.
#:   LastUpdatedAt  `Group.GetInfo()` stamps `DateTime.UtcNow` as it builds the DTO.
#:   LastUpdate     the same, for `PlayQueueUpdate`.
#:   When           the command's scheduled instant: wall clock + a latency cushion.
#:   EmittedAt      the wall clock at which the command was rendered.
#:
#: The group's own GUID is NOT listed: it is normalised by VALUE instead (below),
#: so `GroupLeft`'s payload — which echoes the group id as a DASHED string, an
#: upstream quirk — is still compared rather than skipped.
PUSH_EXTRA_VOLATILE = ("MessageId", "LastUpdatedAt", "LastUpdate", "When", "EmittedAt")
PUSH_VOLATILE = re.compile(
    f"(?:{parity_diff.VOLATILE.pattern})|^({'|'.join(PUSH_EXTRA_VOLATILE)})$")

#: Values under these keys are per-instance GUIDs minted when a queue is built, so
#: they cannot be equal — but they are normalised rather than denylisted, so the
#: EMPTY guid (which both servers send for an empty queue, and which is real
#: information) still compares as itself.
PER_INSTANCE_GUID_KEYS = frozenset({"PlaylistItemId"})
GUID_RE = re.compile(r"^[0-9a-f]{8}-?[0-9a-f]{4}-?[0-9a-f]{4}-?[0-9a-f]{4}-?[0-9a-f]{12}$")


def _is_guid(s):
    return bool(GUID_RE.match(s.lower()))


def _is_empty_guid(s):
    return _is_guid(s) and set(s) <= {"0", "-"}


def normalise(doc, subs, key=None):
    """Replace per-instance identity STRINGS with stable placeholders.

    `subs` maps one server's own group id (dashed AND dashless, lower case) to
    `<group-id>`; that keeps every field carrying it comparable instead of
    dropping the field. A non-empty GUID under a `PER_INSTANCE_GUID_KEYS` key
    becomes `<Key>` — so "both empty" and "both populated" still differ.
    """
    if isinstance(doc, dict):
        return {k: normalise(v, subs, k) for k, v in doc.items()}
    if isinstance(doc, list):
        return [normalise(v, subs, key) for v in doc]
    if isinstance(doc, str):
        low = doc.lower()
        if low in subs:
            return subs[low]
        if key in PER_INSTANCE_GUID_KEYS and _is_guid(low) and not _is_empty_guid(low):
            return f"<{key}>"
    return doc


def msg_key(m):
    """What identifies a pushed message for SEQUENCE comparison.

    The envelope type alone is too coarse: every SyncPlay group update is a
    `SyncPlayGroupUpdate`, so a `GroupJoined` and a `PlayQueue` would look
    interchangeable. The discriminator inside `Data` is what a client switches on.
    """
    data = m.get("Data")
    inner = ""
    if isinstance(data, dict):
        inner = data.get("Type") or data.get("Command") or ""
    return f"{m.get('MessageType')}/{inner}" if inner else str(m.get("MessageType"))


def diff_docs(j, h, subs_j, subs_h):
    """`(n_diffs, compared, paths)` for one pair of documents."""
    out = {"mismatch": [], "missing": [], "extra": []}
    stats = {"compared": 0}
    parity_diff.diff(normalise(j, subs_j), normalise(h, subs_h), "", out, PUSH_VOLATILE, stats)
    paths = [d.get("path", "?") for bucket in out.values() for d in bucket]
    return len(paths), stats["compared"], paths


def compare_pushes(legs):
    """Diff the collected pushes, socket by socket.

    `legs` is a list of `(label, jellyfin_msgs, hermit_msgs, subs_j, subs_h)`.
    Returns `(ok, compared, note)`. `compared` counts the leaf comparisons that
    actually ran, so "nothing was compared" can never be read as "everything
    matched" — the same rule the read layer applies.
    """
    compared = 0
    problems = []
    counts = []
    for label, jm, hm, sj, sh in legs:
        jseq, hseq = [msg_key(m) for m in jm], [msg_key(m) for m in hm]
        counts.append(f"{label} H/J {len(hm)}/{len(jm)}")
        if jseq != hseq:
            problems.append(f"{label}: pushed message sequence differs — H={hseq} J={jseq}")
            continue
        for i, (jmsg, hmsg) in enumerate(zip(jm, hm)):
            n, c, paths = diff_docs(jmsg, hmsg, sj, sh)
            compared += c
            if n:
                problems.append(f"{label}[{i}] {jseq[i]}: {n} diff(s) at {paths[:4]}")
    note = "; ".join(counts)
    if problems:
        return False, compared, f"{note} | " + " | ".join(problems)
    return True, compared, note


# ---------------------------------------------------------------- one server

class Server:
    """One base URL, with its own logins and sockets (nothing module-global)."""

    def __init__(self, base, tag):
        self.base = base.rstrip("/")
        self.tag = tag
        self.sockets = []

    def login(self, device_id):
        ident = dict(client="parity-push", device="parity-push",
                     device_id=f"{device_id}-{self.tag}", version="1")
        st, body = ws_http("POST", "/Users/AuthenticateByName", base=self.base,
                           body={"Username": USER, "Pw": PASS}, **ident)
        if st != 200:
            raise RuntimeError(f"{self.base}: login failed {st}: {body!r}")
        return {"token": body["AccessToken"], "ident": ident,
                "session_id": body["SessionInfo"]["Id"], "user_id": body["User"]["Id"]}

    def http(self, method, path, sess, body=None):
        return ws_http(method, path, base=self.base, token=sess["token"],
                       body=body, **sess["ident"])

    def socket(self, sess):
        ws = WS(f"/socket?api_key={sess['token']}&deviceId={sess['ident']['device_id']}",
                base=self.base)
        self.sockets.append(ws)
        return ws

    def close(self):
        for ws in self.sockets:
            ws.close()
        self.sockets = []


def settle(sockets, seconds=1.2):
    """Let anything in flight land, then clear — so a leg starts from silence.

    Without this the previous verb's broadcast races the next `collect()` and is
    reported as a spurious extra message on whichever server happened to be slower.
    """
    time.sleep(seconds)
    for ws in sockets:
        ws.drain()


def subs_for(group_id):
    """The per-instance group id → a placeholder that REMEMBERS its spelling.

    The two spellings map to two different placeholders on purpose. Jellyfin
    writes the group id dashless in `GroupId` but DASHED in `GroupLeft`'s payload
    (`new SyncPlayGroupLeftUpdate(GroupId, GroupId.ToString())`, Group.cs). Folding
    both into one token would make a server that echoed the wrong spelling there
    compare equal; keeping them apart means the quirk itself is still asserted,
    which denylisting `GroupId` outright could never do.
    """
    if not group_id:
        return {}
    g = uuid.UUID(str(group_id))
    return {str(g): "<group-id-dashed>", g.hex: "<group-id-hex>"}


# ---------------------------------------------------------------- the run

def run(ferrofin_url, jellyfin_url):
    rows, errors = {}, []
    H, J = Server(ferrofin_url, "h"), Server(jellyfin_url, "j")
    stamp = uuid.uuid4().hex[:8]
    try:
        pairs = {}
        for srv in (H, J):
            ctrl = srv.login(f"push-{stamp}-a")
            peer = srv.login(f"push-{stamp}-b")
            pairs[srv.tag] = {"srv": srv, "ctrl": ctrl, "peer": peer,
                              "ws_ctrl": srv.socket(ctrl), "ws_peer": srv.socket(peer)}
        h, j = pairs["h"], pairs["j"]
        allsock = [h["ws_ctrl"], h["ws_peer"], j["ws_ctrl"], j["ws_peer"]]

        # -- POST /SyncPlay/Ping from a session that is in NO group ------------
        # The one playback verb upstream does NOT gate on `SyncPlayIsInGroup`
        # (`SyncPlayController.SyncPlayPing` carries no route policy), so it must
        # be accepted and answered with a `NotInGroup` push rather than a 403.
        # Run FIRST, while neither session has ever joined anything.
        settle(allsock)
        st_h, _ = h["srv"].http("POST", "/SyncPlay/Ping", h["ctrl"], {"Ping": 77})
        st_j, _ = j["srv"].http("POST", "/SyncPlay/Ping", j["ctrl"], {"Ping": 77})
        pings = (h["ws_ctrl"].collect(), j["ws_ctrl"].collect())
        ok, compared, note = compare_pushes(
            [("ping/controller", pings[1], pings[0], {}, {})])
        ok = ok and st_h == st_j
        rows["POST /SyncPlay/Ping"] = verdict(
            ok, compared, f"H={st_h} J={st_j} | {note} | {compared} field(s) compared (pushed messages)",
            extra="the not-in-group leg: accepted (no IsInGroup policy) + a NotInGroup push")

        # -- POST /SyncPlay/New ------------------------------------------------
        settle(allsock)
        new_bodies, gids = {}, {}
        for p in (h, j):
            st, body = p["srv"].http("POST", "/SyncPlay/New", p["ctrl"],
                                     {"GroupName": "  parity push  "})
            new_bodies[p["srv"].tag] = (st, body)
            gids[p["srv"].tag] = body.get("GroupId") if isinstance(body, dict) else None
        pushes = {"h": h["ws_ctrl"].collect(), "j": j["ws_ctrl"].collect()}
        sh, sj = subs_for(gids["h"]), subs_for(gids["j"])
        ok, compared, note = compare_pushes(
            [("new/creator", pushes["j"], pushes["h"], sj, sh)])
        # ...and the response body, which DOES carry signal here (GroupName,
        # State, Participants) — the curated sentence claiming otherwise was wrong.
        (st_h, b_h), (st_j, b_j) = new_bodies["h"], new_bodies["j"]
        n, c, paths = diff_docs(b_j, b_h, sj, sh)
        compared += c
        body_note = f"{c} body" + (f", {n} diff(s) at {paths[:4]}" if n else "")
        rows["POST /SyncPlay/New"] = verdict(
            ok and n == 0 and st_h == st_j == 200, compared,
            f"H={st_h} J={st_j} | {note} | {compared} field(s) compared "
            f"({compared - c} pushed + {body_note})")
        if not gids["h"] or not gids["j"]:
            errors.append("SyncPlay/New did not return a GroupId on both servers")
            return rows, errors

        # -- POST /SyncPlay/Join ----------------------------------------------
        # Two sockets matter: the joiner (GroupJoined + the state hook) and the
        # member already in the group (UserJoined). Both are compared.
        settle(allsock)
        st = {}
        for p, gid in ((h, gids["h"]), (j, gids["j"])):
            st[p["srv"].tag], _ = p["srv"].http("POST", "/SyncPlay/Join", p["peer"],
                                                {"GroupId": gid})
        jn = {t: (pairs[t]["ws_peer"].collect(), pairs[t]["ws_ctrl"].collect())
              for t in ("h", "j")}
        ok, compared, note = compare_pushes([
            ("join/joiner", jn["j"][0], jn["h"][0], sj, sh),
            ("join/member", jn["j"][1], jn["h"][1], sj, sh),
        ])
        rows["POST /SyncPlay/Join"] = verdict(
            ok and st["h"] == st["j"], compared,
            f"H={st['h']} J={st['j']} | {note} | {compared} field(s) compared (pushed messages)")

        # -- GET /SyncPlay/{id} ------------------------------------------------
        # A real JSON body, so this row is body-diff — not the push method.
        #
        # `LastUpdatedAt` cannot be diffed between two instances (sub-ms skew), so
        # it is VOLATILE in the diff — and excluding a field is exactly how a probe
        # can hide a bug. It is therefore asserted separately, and STRUCTURALLY:
        # `Group.GetInfo()` passes `DateTime.UtcNow` into the DTO, so the value
        # MUST ADVANCE between two reads of an unmutated group. A frozen field (the
        # defect Ferrofin had — it returned the last mutation's stamp) cannot pass
        # that, where an absolute age bound would, since a young group's stale
        # stamp is still young.
        settle(allsock, 0.3)
        got, fresh = {}, {}
        for pas in (0, 1):
            for p, gid in ((h, gids["h"]), (j, gids["j"])):
                asked = time.time()
                code, body = p["srv"].http("GET", f"/SyncPlay/{gid}", p["ctrl"])
                got[p["srv"].tag] = (code, body)
                fresh.setdefault(p["srv"].tag, []).append(stamp_age(body, asked))
            if not pas:
                time.sleep(0.5)
        n, c, paths = diff_docs(got["j"][1], got["h"][1], sj, sh)
        # Frozen if the second read's stamp is no closer to "now" than the first's
        # was — a stamp that tracks the clock gets younger relative to each read.
        stale = [t for t, ages in fresh.items()
                 if None in ages or ages[1] > 1.0 or ages[1] >= ages[0] + 0.4]
        rows["GET /SyncPlay/{id}"] = verdict(
            n == 0 and not stale and got["h"][0] == got["j"][0] == 200, c,
            f"H={got['h'][0]} J={got['j'][0]} | {c} field(s) compared"
            + (f", {n} diff(s) at {paths[:4]}" if n else "")
            + " | LastUpdatedAt advances per read: "
            + " ".join(f"{t.upper()}={'/'.join(fmt_age(a) for a in ages)}"
                       for t, ages in sorted(fresh.items()))
            + (f" — FROZEN on {stale}" if stale else ""),
            method=verification.BODY_DIFF)

        # -- POST /SyncPlay/Leave ---------------------------------------------
        # The leaver gets `GroupLeft` (whose payload echoes the group id in the
        # DASHED spelling — an upstream quirk that the value normalisation keeps
        # comparable), everyone else gets `UserLeft`.
        settle(allsock)
        st = {}
        for p in (h, j):
            st[p["srv"].tag], _ = p["srv"].http("POST", "/SyncPlay/Leave", p["peer"])
        lv = {t: (pairs[t]["ws_peer"].collect(), pairs[t]["ws_ctrl"].collect())
              for t in ("h", "j")}
        ok, compared, note = compare_pushes([
            ("leave/leaver", lv["j"][0], lv["h"][0], sj, sh),
            ("leave/member", lv["j"][1], lv["h"][1], sj, sh),
        ])
        rows["POST /SyncPlay/Leave"] = verdict(
            ok and st["h"] == st["j"], compared,
            f"H={st['h']} J={st['j']} | {note} | {compared} field(s) compared (pushed messages)")

        # -- tear the groups down so the lab is left as it was found -----------
        for p in (h, j):
            p["srv"].http("POST", "/SyncPlay/Leave", p["ctrl"])
    except Exception as e:                                   # noqa: BLE001
        errors.append(f"{type(e).__name__}: {e}")
    finally:
        H.close()
        J.close()
    return rows, errors


def stamp_age(body, asked):
    """Seconds between `LastUpdatedAt` and when the read was issued, or None."""
    if not isinstance(body, dict) or not body.get("LastUpdatedAt"):
        return None
    import datetime
    raw = body["LastUpdatedAt"].rstrip("Z")
    if "." in raw:                       # .NET writes up to 7 fractional digits
        head, frac = raw.split(".", 1)
        raw = f"{head}.{frac[:6]}"
    try:
        t = datetime.datetime.fromisoformat(raw).replace(tzinfo=datetime.timezone.utc)
    except ValueError:
        return None
    return abs(t.timestamp() - asked)


def fmt_age(age):
    return "n/a" if age is None else f"{age:.1f}s"


def verdict(ok, compared, note, method=None, extra=None):
    """One results row.

    Three outcomes, never two: nothing compared is UNTESTED (no verdict, no
    method — a probe that measured nothing must not claim a result), a clean
    comparison earns its method, and anything else is a flagged red.
    """
    if not compared:
        return {"deep_verified": None, "verification_method": None,
                "note": f"nothing compared — {note}",
                "classification": "flagged: the push probe compared no fields"}
    full = note + (f" ({extra})" if extra else "")
    if ok:
        return {"deep_verified": True,
                "verification_method": method or verification.PUSH_DIFF,
                "note": full, "classification": "ok"}
    return {"deep_verified": False, "verification_method": method or verification.PUSH_DIFF,
            "note": full,
            "classification": "flagged: pushed messages or response differ (verify against the C#)"}


def main():
    if "--check" in sys.argv:
        selfcheck()
        return
    rows, errors = run(os.environ.get("FERROFIN_URL", "http://localhost:18096"),
                       os.environ.get("JELLYFIN_URL", "http://localhost:18097"))
    out = {"generated_by": "suite/parity/push.py",
           "last_verified": os.environ.get("PARITY_STAMP", ""),
           "errors": errors, "rows": rows}
    with open(os.path.join(ROOT, "suite/parity/push-results.json"), "w") as f:
        json.dump(out, f, indent=2, sort_keys=True)
        f.write("\n")
    ok = sum(1 for v in rows.values() if v["deep_verified"])
    print(f"wrote parity/push-results.json — {len(rows)} op(s), {ok} verified"
          + (f", errors: {errors}" if errors else ""))
    for k, v in sorted(rows.items()):
        print(f"  {v['deep_verified']!s:>5} {v['verification_method'] or '-':<10} {k}: {v['note']}")


def selfcheck():
    """Prove the differ REJECTS. A differ nobody has seen fail is not a differ."""
    def env(mtype, data):
        return {"MessageType": mtype, "MessageId": uuid.uuid4().hex, "Data": data}

    gj = lambda gid: env("SyncPlayGroupUpdate", {                       # noqa: E731
        "Type": "GroupJoined", "GroupId": gid,
        "Data": {"GroupId": gid, "GroupName": "g", "State": "Idle",
                 "Participants": ["bench"], "LastUpdatedAt": "2026-01-01T00:00:00Z"}})
    stop = lambda gid, when: env("SyncPlayCommand", {                   # noqa: E731
        "GroupId": gid, "PlaylistItemId": "00000000000000000000000000000000",
        "When": when, "PositionTicks": 0, "Command": "Stop", "EmittedAt": when})
    GH, GJ = uuid.uuid4().hex, uuid.uuid4().hex
    sh, sj = subs_for(GH), subs_for(GJ)

    # 1. identical traffic modulo per-instance ids/timestamps -> clean, and it
    #    really compared something.
    ok, n, note = compare_pushes([("x", [gj(GJ), stop(GJ, "2026-01-01T00:00:01Z")],
                                       [gj(GH), stop(GH, "2026-01-01T00:00:02Z")], sj, sh)])
    assert ok and n > 5, (ok, n, note)

    # 2. THE BUG THIS LAYER WAS BUILT FOR: Jellyfin pushes two, Ferrofin one.
    ok, n, note = compare_pushes([("x", [gj(GJ), stop(GJ, "t")], [gj(GH)], sj, sh)])
    assert not ok and "sequence differs" in note, note

    # 3. ...and the mirror image: an EXTRA message on Ferrofin is equally red.
    ok, _, note = compare_pushes([("x", [gj(GJ)], [gj(GH), stop(GH, "t")], sj, sh)])
    assert not ok and "sequence differs" in note, note

    # 4. nothing arrived on either side: no fields compared -> untested, never a pass.
    ok, n, note = compare_pushes([("x", [], [], {}, {})])
    assert ok and n == 0, (ok, n)
    assert verdict(ok, n, note)["deep_verified"] is None
    assert verdict(ok, n, note)["verification_method"] is None

    # 5. same message types, a payload field that genuinely differs -> red.
    bad = gj(GH)
    bad["Data"]["Data"]["State"] = "Playing"
    ok, _, note = compare_pushes([("x", [gj(GJ)], [bad], sj, sh)])
    assert not ok and "State" in note, note

    # 6. the same field being VOLATILE must not launder a real difference in a
    #    sibling field: MessageId/When are skipped, GroupName is not.
    bad = gj(GH)
    bad["Data"]["Data"]["GroupName"] = "other"
    ok, _, note = compare_pushes([("x", [gj(GJ)], [bad], sj, sh)])
    assert not ok and "GroupName" in note, note

    # 7. the group id is NORMALISED, not skipped — so `GroupLeft`, whose payload
    #    is the group id as a DASHED string, still asserts that quirk. A server
    #    echoing the DASHLESS form there is a difference.
    left = lambda gid, data: env("SyncPlayGroupUpdate",                 # noqa: E731
                                 {"Type": "GroupLeft", "GroupId": gid, "Data": data})
    ok, n, _ = compare_pushes([("x", [left(GJ, str(uuid.UUID(GJ)))],
                                    [left(GH, str(uuid.UUID(GH)))], sj, sh)])
    assert ok and n >= 2
    ok, _, note = compare_pushes([("x", [left(GJ, str(uuid.UUID(GJ)))],
                                       [left(GH, GH)], sj, sh)])
    assert not ok, note

    # 8. PlaylistItemId is normalised, not denylisted: two independent non-empty
    #    ids agree, but "one server has a playing item and the other does not" is
    #    a difference, not a wash.
    zero = "00000000000000000000000000000000"
    ok, _, _ = compare_pushes([("x", [stop(GJ, "t")], [stop(GH, "t")], sj, sh)])
    assert ok
    a, b = stop(GJ, "t"), stop(GH, "t")
    a["Data"]["PlaylistItemId"] = uuid.uuid4().hex
    b["Data"]["PlaylistItemId"] = uuid.uuid4().hex
    ok, _, _ = compare_pushes([("x", [a], [b], sj, sh)])
    assert ok, "two independently-minted playlist item ids must not be a diff"
    b["Data"]["PlaylistItemId"] = zero
    ok, _, note = compare_pushes([("x", [a], [b], sj, sh)])
    assert not ok and "PlaylistItemId" in note, note

    # 9. the method this layer stamps is in the closed set, and is NOT the headline.
    assert verification.PUSH_DIFF in verification.VALID
    assert verification.PUSH_DIFF != verification.HEADLINE
    row = verdict(True, 8, "n")
    assert row["verification_method"] == verification.PUSH_DIFF
    assert verdict(True, 3, "n", method=verification.BODY_DIFF)["verification_method"] \
        == verification.BODY_DIFF
    assert verdict(False, 8, "n")["deep_verified"] is False

    # 10. the freshness property used for GET /SyncPlay/{id}.
    now = time.time()
    import datetime
    iso = datetime.datetime.fromtimestamp(now, datetime.timezone.utc).isoformat()
    assert stamp_age({"LastUpdatedAt": iso.replace("+00:00", "Z")}, now) < 1
    assert stamp_age({"LastUpdatedAt": "2020-01-01T00:00:00.1234567Z"}, now) > 30
    assert stamp_age({}, now) is None

    print("ok: push differential rejects a missing message, an extra message, a "
          "changed payload field and a lost playing item; an empty capture is "
          f"untested, not verified; stamps {verification.PUSH_DIFF!r} "
          f"(headline stays {verification.HEADLINE!r})")


if __name__ == "__main__":
    main()
