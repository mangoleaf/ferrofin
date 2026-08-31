#!/usr/bin/env python3
"""Layer-2 PUSH differential: the server→client WebSocket messages an op causes.

Some operations say almost nothing in their HTTP response — `POST /SyncPlay/Join`
answers `204` with a zero-byte body — and everything they actually DO arrives
later, over the client's `/socket`, as a `SyncPlayGroupUpdate` or a
`SyncPlayCommand`. No HTTP-body layer can see that, so 21 SyncPlay ops sat in the
ledger wearing a curated sentence ("the response body carries no signal here")
that was never earned by a measurement, and the sentence hid a real port gap:
Jellyfin pushed TWO messages where Ferrofin pushed one.

WHAT THIS LAYER COVERS, exactly — the previous version of this sentence claimed
"all thirteen SyncPlay ops through every state of the machine" and a reviewer had
to measure that it was false for four verbs. Of the vendored contract's 22
SyncPlay operations it drives 20: the seven playback verbs (Pause, Unpause, Stop,
Seek, Buffering, Ready, SetIgnoreWait) and SetNewQueue from all four group
states; SetPlaylistItem, NextItem, PreviousItem, Queue, RemoveFromPlaylist and
MovePlaylistItem through every branch of their single `AbstractGroupState` arm,
including the one that pushes nothing and only changes the state; and New, Join,
Leave, Ping, `GET /SyncPlay/{id}` and `GET /SyncPlay/List`, which have no state
machine. It does NOT drive SetRepeatMode or SetShuffleMode: `PlayQueueManager`
shuffles with `OrderBy(_ => Guid.NewGuid())`, so even a correct implementation
produces an order two instances cannot match, and comparing them needs a set-wise
strategy this layer does not have. Those two rows are unprobed, and named so,
rather than driven in a way that could only be green by comparing nothing.

This layer opens real sockets on BOTH servers, issues the same op against each,
collects what each server pushed, and compares them:

  * the ordered SEQUENCE of messages per receiving socket — which types arrived
    and how many. "Jellyfin sent two and Ferrofin sent one" is the finding this
    layer exists to produce, and an absent message is reported as absent, never
    quietly folded into agreement;
  * every non-volatile field of each matched message's payload, through the same
    `parity_diff` the read layer uses;
  * the two wall-clock instants a field diff CANNOT compare — a command's `When`
    and `EmittedAt` — through their derived offset `When − EmittedAt`, which each
    server computes from its own clock. `When` is the instant the client is told
    to act on, so denylisting it with nothing in its place would have left the
    most behaviourally loaded field in a `SyncPlayCommand` unasserted;
  * plus, where the op returns a body, that body — diffed the same way.

Everything the probe drains but does not compare is REPORTED, never swallowed:
the settle windows between legs record what they drop, and `residue_report()`
turns that into an error (a late SyncPlay frame means its leg was mismeasured) or
an observation (a socket-lifecycle frame — and one only Jellyfin sends is written
down as a divergence, since `/socket` is not a contract op and can own no row).

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

#: The SECOND session logs in as its own user, created and deleted by this layer.
#:
#: Two sessions of the SAME user make `Participants` a one-element list on both
#: servers, and a one-element list cannot tell `Group.GetInfo()`'s LINQ
#: `Distinct()` (first-join order) apart from a sorted list — so the ordering
#: would be unit-tested only and never differentiated live. The name is chosen to
#: sort BEFORE the admin's: joining second, it must still come SECOND, which is
#: false for any implementation that sorts.
JOINER_USER = "aparityjoiner"
JOINER_PASS = "Parity!123"

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
#: `When` and `EmittedAt` are NOT simply dropped. `When` is the most
#: behaviourally loaded field in a `SyncPlayCommand` — it is the instant the
#: client is told to act on — so denylisting it with nothing in its place would
#: leave a server that stamped `When = now` indistinguishable from a correct one.
#: It is replaced STRUCTURALLY by `when_offset()` below, the same way
#: `LastUpdatedAt` is replaced by the freshness assertion on GET /SyncPlay/{id}.
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


#: How far the two servers' `When − EmittedAt` offsets may differ before it is a
#: finding, in seconds. The probe issues the two servers' HTTP calls back to
#: back, so the only legitimate gap between their offsets is one round trip —
#: tens of milliseconds (6ms measured on this lab). 0.5s is an order of magnitude
#: of headroom over that, and still an order of magnitude below the multi-second
#: error it exists to catch (a group seconds old whose command was stamped `now`).
WHEN_OFFSET_TOLERANCE = 0.5


def parse_time(raw):
    """One .NET/RFC3339 UTC instant as a POSIX timestamp, or None.

    .NET writes up to SEVEN fractional digits, which `fromisoformat` rejects on
    the Python we pin, so the fraction is trimmed to microseconds.
    """
    import datetime
    if not isinstance(raw, str) or not raw:
        return None
    raw = raw.rstrip("Z")
    if "." in raw:
        head, frac = raw.split(".", 1)
        raw = f"{head}.{frac[:6]}"
    try:
        t = datetime.datetime.fromisoformat(raw)
    except ValueError:
        return None
    if t.tzinfo is None:
        t = t.replace(tzinfo=datetime.timezone.utc)
    return t.timestamp()


def when_offset(m):
    """`When − EmittedAt` in seconds for a `SyncPlayCommand`, else None.

    THE STRUCTURAL REPLACEMENT FOR THE DENYLISTED `When`. Both fields are
    absolute wall clocks that two independent instances cannot match, but their
    DIFFERENCE is computed by each server from its own single clock, so it IS
    comparable — and it is exactly where the meaning of `When` lives.

    Upstream builds a command as `When = context.LastActivity`, `EmittedAt =
    DateTime.UtcNow` (`Group.NewSyncPlayCommand`,
    v10.11.8:Emby.Server.Implementations/SyncPlay/Group.cs:317-327), so for a
    Stop on a group that has been idle N seconds the offset is about −N on both
    servers. A server that stamped `When = now` instead of the group's
    `LastActivity` would report ≈0 — a difference the field-by-field diff can
    never see, because it never looks at either field.

    Returns None when the message carries neither (every `SyncPlayGroupUpdate`),
    so a message pair with no command semantics is simply not asserted.
    """
    data = m.get("Data")
    if not isinstance(data, dict):
        return None
    when, emitted = parse_time(data.get("When")), parse_time(data.get("EmittedAt"))
    if when is None or emitted is None:
        return None
    return when - emitted


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


def diff_docs(j, h, subs_j, subs_h, softened=None):
    """`(n_diffs, compared, paths)` for one pair of documents.

    `softened` collects the playback positions compared with a tolerance rather
    than exactly (see `soften_positions`), so the caller can print them.
    """
    out = {"mismatch": [], "missing": [], "extra": []}
    stats = {"compared": 0}
    nj, nh = normalise(j, subs_j), normalise(h, subs_h)
    soften_positions(nj, nh, softened if softened is not None else [])
    parity_diff.diff(nj, nh, "", out, PUSH_VOLATILE, stats)
    paths = [d.get("path", "?") for bucket in out.values() for d in bucket]
    return len(paths), stats["compared"], paths


def compare_pushes(legs):
    """Diff the collected pushes, socket by socket.

    `legs` is a list of `(label, jellyfin_msgs, hermit_msgs, subs_j, subs_h)`.
    Returns `(ok, compared, note)`. `compared` counts the leaf comparisons that
    actually ran, so "nothing was compared" can never be read as "everything
    matched" — the same rule the read layer applies. `ok` is independent of it:
    a sequence mismatch leaves NOTHING to compare field-by-field and is still a
    definite failure, which is why `verdict()` weighs `ok` first.
    """
    compared = 0
    problems = []
    counts = []
    offsets = []
    softened = []
    for label, jm, hm, sj, sh in legs:
        jseq, hseq = [msg_key(m) for m in jm], [msg_key(m) for m in hm]
        counts.append(f"{label} H/J {len(hm)}/{len(jm)}")
        if jseq != hseq:
            problems.append(f"{label}: pushed message sequence differs — H={hseq} J={jseq}")
            continue
        for i, (jmsg, hmsg) in enumerate(zip(jm, hm)):
            n, c, paths = diff_docs(jmsg, hmsg, sj, sh, softened)
            compared += c
            if n:
                problems.append(f"{label}[{i}] {jseq[i]}: {n} diff(s) at {paths[:4]}")
            # ...and `When`, which the field diff skipped, asserted through the
            # only form of it that CAN cross two instances (see `when_offset`).
            oj, oh = when_offset(jmsg), when_offset(hmsg)
            if (oj is None) != (oh is None):
                problems.append(
                    f"{label}[{i}] {jseq[i]}: only one server carries When+EmittedAt "
                    f"(H={'yes' if oh is not None else 'no'} J={'yes' if oj is not None else 'no'})")
            elif oj is not None:
                compared += 1
                offsets.append(f"{jseq[i]} H={oh:+.3f}s J={oj:+.3f}s")
                if abs(oj - oh) > WHEN_OFFSET_TOLERANCE:
                    problems.append(
                        f"{label}[{i}] {jseq[i]}: When−EmittedAt offset differs — "
                        f"H={oh:+.3f}s J={oj:+.3f}s (> {WHEN_OFFSET_TOLERANCE}s)")
    note = "; ".join(counts)
    if offsets:
        note += " | When−EmittedAt: " + "; ".join(offsets)
    if softened:
        note += " | position (tolerance ±%.2fs): " % (POSITION_TOLERANCE_TICKS / 10_000_000)
        note += "; ".join(sorted(set(softened)))
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

    def login(self, device_id, user=USER, password=PASS):
        ident = dict(client="parity-push", device="parity-push",
                     device_id=f"{device_id}-{self.tag}", version="1")
        st, body = ws_http("POST", "/Users/AuthenticateByName", base=self.base,
                           body={"Username": user, "Pw": password}, **ident)
        if st != 200:
            raise RuntimeError(f"{self.base}: login as {user!r} failed {st}: {body!r}")
        return {"token": body["AccessToken"], "ident": ident,
                "session_id": body["SessionInfo"]["Id"], "user_id": body["User"]["Id"]}

    def ensure_user(self, admin, name, password):
        """Create `name` fresh and return its id (`POST /Users/New`).

        A stale account from an aborted run is deleted first — `Users/New` refuses
        a duplicate name, and journeys.py hit exactly that. The account is deleted
        again in `run()`'s `finally`, symmetrically on both servers, so the layer
        leaves the lab as it found it.
        """
        for u in (self.http("GET", "/Users", admin)[1] or []):
            if isinstance(u, dict) and u.get("Name") == name:
                self.http("DELETE", f"/Users/{u['Id']}", admin)
        st, body = self.http("POST", "/Users/New", admin,
                             {"Name": name, "Password": password})
        if st >= 300 or not isinstance(body, dict) or not body.get("Id"):
            raise RuntimeError(f"{self.base}: could not create {name!r}: {st} {body!r}")
        return body["Id"]

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


def settle(sockets, residue, seconds=1.2):
    """Let anything in flight land, then clear — so a leg starts from silence.

    Without this the previous verb's broadcast races the next `collect()` and is
    reported as a spurious extra message on whichever server happened to be slower.

    NOTHING IS SILENTLY DISCARDED. Every frame drained here is recorded into
    `residue`, tagged with the socket it arrived on, and reported by
    `residue_report()` at the end of the run. A settle window that swallows what
    it drains is a laundering machine: it folds anything late into agreement. It
    is also how this layer was structurally blind to a divergence it walks past
    twice per run — the `ForceKeepAlive` cadence, which Ferrofin ran as a flat
    60 s metronome where upstream runs an inactivity watchdog — and it is how a
    genuinely delayed SyncPlay message on a future leg would vanish instead of
    failing the row.

    `sockets` is a list of `(label, ws)`; the label is what makes a leftover
    attributable to a server and a socket rather than to "somewhere".
    """
    time.sleep(seconds)
    for label, ws in sockets:
        for m in ws.drain():
            residue.append((label, msg_key(m)))
        # The keep-alive frames the client kept out of the compared sets (see
        # `wsclient.LIFECYCLE_MESSAGES`) are reported here exactly as before, so
        # the per-server count difference stays visible.
        for m in ws.drain_lifecycle():
            residue.append((label, msg_key(m)))


#: Message types that belong to the SOCKET's own lifecycle rather than to any
#: operation this layer drives. They are still reported (see `residue_report`) —
#: this set only decides whether a leftover invalidates the run's rows.
SOCKET_LIFECYCLE = frozenset({"ForceKeepAlive", "KeepAlive"})

#: Server-initiated broadcasts that NO SyncPlay verb can cause, and that arrive
#: on a schedule of the server's own — so one landing inside a leg's collect
#: window is an accident of timing, not that leg's output.
#:
#: `ScheduledTaskEnded` is the one this lab actually produces: both servers
#: implement it (`TaskCompletedNotifier` -> `SendMessageToAdminSessions`, ported
#: in `ferrofin-core::scheduled_tasks`), but their task triggers were seeded at
#: their own container start times, so a run of a few minutes catches a task
#: finishing on one side and not the other. Attributing that to whichever
#: SyncPlay verb happened to be mid-leg is a FALSE RED, and re-running until it
#: does not happen is worse.
#:
#: The set is deliberately tiny and grows only for a type that has been OBSERVED
#: here and that no SyncPlay arm in the C# can emit. Anything not listed still
#: fails its row: a server pushing an unexpected message during an op is exactly
#: what this layer is for. Every frame taken out this way is counted and printed
#: by `residue_report`, per server — nothing disappears.
BACKGROUND_BROADCASTS = frozenset({"ScheduledTaskEnded"})

#: How long `prove_library_changed` waits for the debounced push. Both
#: servers default `LibraryUpdateDuration` to 30 s and each restarts its
#: timer on its own last write, so this is that plus margin.
LIBRARY_UPDATE_WAIT = 45

#: Everything a leftover may be without invalidating the run's rows.
UNATTRIBUTABLE = SOCKET_LIFECYCLE | BACKGROUND_BROADCASTS

#: Message types this RUN proved both servers implement (see
#: `prove_library_changed`). Empty until a proof succeeds, and never seeded from
#: a literal: the whole point is that the allowance expires with the run that
#: earned it. `sift` reads it too, because a frame landing inside a leg's
#: collect window is the same frame as one landing between legs.
PROVEN = set()


def sift(msgs, label, residue):
    """`msgs` minus the frames no op here could have caused, which go to `residue`.

    Called on every compared capture, not only on the settle windows: a
    background broadcast that lands INSIDE a leg's collect window is the same
    frame as one that lands between legs, and it must be attributed the same way
    — counted and reported, never compared as though the verb produced it.
    """
    kept = []
    for m in msgs:
        if m.get("MessageType") in UNATTRIBUTABLE or m.get("MessageType") in PROVEN:
            residue.append((label, msg_key(m)))
        else:
            kept.append(m)
    return kept


def prove_library_changed(pairs, sockets, residue):
    """Make each server PROVE it pushes `LibraryChanged`, and only then agree to
    ignore the stray ones.

    Why this exists instead of a name in `BACKGROUND_BROADCASTS`. That set means
    "no op here can cause this, so a leftover is timing" — which is only true if
    BOTH servers actually implement the message. `LibraryChanged` was withdrawing
    every row of this layer at j=1/h=0, and listing it would have converted a
    push Ferrofin was not sending into 20 verified rows. But leaving it listed
    once Ferrofin DOES send it would be equally wrong in the other direction:
    the exclusion would then rest on a claim nobody re-checks, and a regression
    that silenced the push would look exactly like a quiet run.

    So the allowance is earned per run: edit an item on each server, wait out
    `LibraryUpdateDuration`, and require the push on both. A server that stays
    silent keeps `LibraryChanged` an error and this layer's rows stay withdrawn,
    which is what an unported notifier deserves.

    Returns `(proven, detail)`. Everything drained here still lands in `residue`.
    """
    mutated, seen = [], {}
    for tag, p_ in pairs.items():
        srv, sess = p_["srv"], p_["ctrl"]
        _, listing = srv.http(
            "GET", "/Items?includeItemTypes=Movie&recursive=true&limit=1"
                   "&userId=%s" % sess["user_id"], sess)
        items = (listing or {}).get("Items") or []
        if not items:
            return False, "%s: no Movie to edit, so the push could not be provoked" % tag
        mid = items[0].get("Id")
        _, dto = srv.http("GET", "/Items/%s" % mid, sess)
        if not isinstance(dto, dict):
            return False, "%s: could not read the item DTO to edit" % tag
        before = list(dto.get("Tags") or [])
        dto["Tags"] = before + ["parity-push-probe"]
        srv.http("POST", "/Items/%s" % mid, sess, dto)
        mutated.append((srv, sess, mid, dto, before))

    # Both debounces are `LibraryUpdateDuration` (30 s by default on both
    # servers), and each restarts on its own last write, so the wait is that
    # plus enough margin for the fold and the socket write.
    deadline = time.time() + LIBRARY_UPDATE_WAIT
    while time.time() < deadline and len(seen) < len(pairs):
        time.sleep(2)
        for label, ws in sockets:
            for m in ws.drain():
                residue.append((label, msg_key(m)))
                if msg_key(m).split("/", 1)[0] == "LibraryChanged":
                    seen[label.split("/", 1)[0]] = True
            for m in ws.drain_lifecycle():
                residue.append((label, msg_key(m)))

    # Put the corpus back the way it was found — every other layer diffs it.
    for srv, sess, mid, dto, before in mutated:
        dto["Tags"] = before
        srv.http("POST", "/Items/%s" % mid, sess, dto)

    missing = sorted({"h", "j"} - set(seen))
    if missing:
        return False, ("no `LibraryChanged` from %s within %ds of an item edit — "
                       "the notifier is not pushing there, so its stray frames "
                       "stay attributable and this layer's rows stay withdrawn"
                       % ("/".join(missing), LIBRARY_UPDATE_WAIT))
    PROVEN.add("LibraryChanged")
    return True, "both servers pushed `LibraryChanged` after an item edit"


def residue_report(residue, proven=frozenset()):
    """`(errors, observations)` for everything the settle windows drained.

    Two rules, both about never letting a frame disappear:

    * A leftover that is neither socket lifecycle nor a background broadcast — a
      SyncPlay update or command that arrived late enough to miss its own leg —
      is an ERROR. The leg it belongs
      to was not measured against a complete capture, so the run's rows are not
      trustworthy and the run must say so rather than publish them quietly.
    * Everything else is an OBSERVATION with per-server counts. An observation
      whose counts DIFFER between the two servers is a real cross-server
      divergence that this layer measured but owns no ledger row for: `/socket`
      is not an operation in the vendored contract, so gen-ledger.py has nowhere
      to put it. Reporting it here is the difference between a known divergence
      and a forgotten one.
    """
    per_server = {}
    for label, key in residue:
        tag = label.split("/", 1)[0]
        per_server.setdefault(key, {}).setdefault(tag, 0)
        per_server[key][tag] += 1
    errors, observations = [], []
    for key, counts in sorted(per_server.items()):
        base = key.split("/", 1)[0]
        shown = f"{key}: " + ", ".join(f"{t}={n}" for t, n in sorted(counts.items()))
        if base not in (UNATTRIBUTABLE | proven):
            errors.append(
                f"a non-lifecycle message was drained by a settle window, so the "
                f"leg after it was measured against an incomplete capture — {shown}")
        elif base in BACKGROUND_BROADCASTS or base in proven:
            observations.append(
                f"BACKGROUND BROADCAST (not caused by any op this layer drives, so "
                f"excluded from the compared sets and counted here instead) — {shown}. "
                f"Both servers implement it; the counts differ because their task "
                f"schedulers were seeded at their own start times. A count on ONE "
                f"side only is timing, not a missing feature — a server that never "
                f"sent one across many runs would be.")
        elif counts.get("h", 0) != counts.get("j", 0):
            observations.append(
                f"COUNT DELTA (socket lifecycle, no contract op owns it) — {shown}. "
                f"This USED to be a standing divergence, recorded here so it is not "
                f"rediscovered as a mystery: Ferrofin served `/socket` with a flat "
                f"60 s `ForceKeepAlive` metronome and no connect-time frame, where "
                f"`SessionWebSocketListener` sends one on connect and then runs an "
                f"INACTIVITY watchdog — every `IntervalFactor * WebSocketLostTimeout` "
                f"(12 s) it prompts any socket silent for more than "
                f"`ForceKeepAliveFactor * WebSocketLostTimeout` (45 s), and a socket "
                f"past the full 60 s leaves the watchlist. This line read h=8 j=14. "
                f"Both halves are now ported, and the frames are frame-for-frame "
                f"identical: measured on one socket per server for 160 s, "
                f"ForceKeepAlive arrived at [0.2, 48.0, 96.0, 144.1] on Ferrofin and "
                f"[0.2, 48.0, 96.1, 144.1] on Jellyfin. So a delta of more than ONE "
                f"per socket is now a regression to investigate, not phase; one is "
                f"a frame landing either side of the end of the run.")
        else:
            observations.append(f"agreed (socket lifecycle) — {shown}")
    return errors, observations


def withdraw_on_incomplete(rows, res_errors):
    """Take back every TRUE verdict when the run's capture was incomplete.

    A non-lifecycle frame in a settle window means at least one leg was collected
    short, and a short capture can produce a FALSE GREEN: if both servers were
    equally late, the leg diffed a truncated message set on each and agreed. So
    the verdicts are withdrawn to untested. Reds stand — a difference the probe
    actually saw is still a difference — and nothing here can turn a red green.

    The withdrawal, not an exit code, is the enforcement: sweep.sh runs under
    `set -e`, so failing the process would abort the whole parity leg and write
    no ledger at all, which is strictly LESS visible than a row that says in
    LEDGER.md why it has no verdict.
    """
    if not res_errors:
        return rows
    for v in rows.values():
        if v["deep_verified"] is True:
            v.update(deep_verified=None, verification_method=None,
                     classification="flagged: a settle window drained a "
                                    "non-lifecycle message, so this run's "
                                    "capture was incomplete",
                     note="withdrawn (incomplete capture) — " + v["note"])
    return rows


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


#: Ticks (100 ns) two independent instances' PLAYBACK POSITION may differ by
#: before it counts as a finding. See `soften_positions`.
#:
#: SIZED FROM THE MEASUREMENT, not from a round number. Across every live run of
#: this layer the largest gap actually observed between the two servers' rendered
#: positions was 2 ms, and the fixture's movies are only ~1.02 s long, so the
#: 0.25 s this started at was ~100x the worst real gap and ~24% of an entire
#: item — wide enough for a systematic 100 ms position error to pass silently,
#: which is the exact failure this layer exists to catch. 20 ms is ten times the
#: worst measured gap and 2% of the item.
POSITION_TOLERANCE_TICKS = 200_000          # 20 ms
#: The payload keys that carry a playback position.
TIME_DERIVED_TICK_KEYS = frozenset({"PositionTicks", "StartPositionTicks"})


def soften_positions(j, h, seen):
    """Collapse a pair of RUNNING playback positions to one placeholder.

    A group that is actually playing advances `PositionTicks` with the wall
    clock (`PositionTicks += now - LastActivity`), so a command rendered after N
    seconds of playback carries a number two independent instances cannot match
    exactly — the same problem `When` has, for the same reason.

    It is NOT denylisted. The two values are compared AGAINST EACH OTHER with a
    tolerance sized at ten times the largest gap ever measured here (see
    `POSITION_TOLERANCE_TICKS`), and anything outside it stays a diff — so the clamp defect
    this layer measures (a server echoing the requested 999_999_999 where the
    oracle clamps to the item's run time) is still caught, loudly. A ZERO on
    either side is never softened either: "one server thinks playback is at the
    start" is exactly the kind of divergence the row exists to find.

    Every softened pair is recorded into `seen` so the note can print the actual
    delta — a tolerance whose gap is invisible is indistinguishable from a
    denylist, and this layer's whole point is that nothing disappears quietly.
    """
    if isinstance(j, dict) and isinstance(h, dict):
        for k in j.keys() & h.keys():
            if (k in TIME_DERIVED_TICK_KEYS
                    and isinstance(j[k], int) and isinstance(h[k], int)
                    and not isinstance(j[k], bool) and not isinstance(h[k], bool)
                    and j[k] and h[k] and abs(j[k] - h[k]) <= POSITION_TOLERANCE_TICKS):
                seen.append(f"{k} H={h[k]} J={j[k]} Δ={(h[k] - j[k]) / 10_000:+.0f}ms")
                j[k] = h[k] = "<position-while-playing>"
            else:
                soften_positions(j[k], h[k], seen)
    elif isinstance(j, list) and isinstance(h, list):
        for a, b in zip(j, h):
            soften_positions(a, b, seen)


def playback_commands(pairs, gids, subs, allsock, residue, rows):
    """Drive every SyncPlay verb through every state of the group state machine
    and compare what each server pushed, on BOTH sockets.

    Upstream does not have one handler per verb: it has one per (state, verb)
    pair — `IdleGroupState`, `WaitingGroupState`, `PlayingGroupState`,
    `PausedGroupState` — and the arms differ in what they send, to WHOM, and
    whether they change state at all. A probe that only ever pauses an idle group
    proves nothing about Pause. So each verb below is issued from every state it
    HAS a distinct arm in, and every leg compares the requesting socket AND the
    peer socket: with one member in the group, `CurrentSession`, `AllGroup` and
    `AllReady` are indistinguishable on the wire, which is how four wrong arms
    survived unnoticed.

    What that means precisely, because the previous version of this docstring
    over-claimed and a reviewer had to measure the gap:

    * Pause, Unpause, Stop, Seek, Buffering, Ready and SetNewQueue are each
      issued from all four states. SetIgnoreWait is issued from all four too,
      though `AbstractGroupState.cs:207-211` gives it only two distinct arms
      (Waiting, and "record the flag and say nothing" everywhere else).
    * SetPlaylistItem, NextItem, PreviousItem, Queue, RemoveFromPlaylist and
      MovePlaylistItem live on `AbstractGroupState`, so upstream has ONE arm each
      and it is state-independent; they are driven from the states that make
      their branches reachable (a step with nowhere to go, a removal that does or
      does not take the playing item) rather than from all four for its own sake.
    * `New`, `Join`, `Leave`, `Ping` and `GET /SyncPlay/{id}` have no state
      machine and are driven in `run()`.
    * SetRepeatMode and SetShuffleMode are NOT driven here — see the module note
      in `run()`: `PlayQueueManager.Shuffle` reorders the playlist with
      `OrderBy(_ => Guid.NewGuid())`, so a correct implementation cannot be
      order-compared between two instances, and Ferrofin does not implement the
      reordering at all. Probing them would need a set-wise comparison this layer
      does not have; they are named as unprobed rather than driven dishonestly.

    Three preconditions are asserted per leg, because a push comparison alone can
    be fooled:

    * the two groups must ALREADY be in the same state before the verb — the
      servers are never driven separately to "the same" state, and a disagreement
      is recorded as a finding on that verb rather than papered over;
    * the group's `State` after the verb must agree. Several arms (Pause while
      waiting, Seek while idle) push a message and deliberately do NOT change
      state, so the message set alone cannot see them; and one arm this layer now
      drives on purpose — a queue change that cannot be applied — differs ONLY in
      the resulting state (`WaitingGroupState`'s `prevState switch` drops a
      waiting group to `Idle` instead of restoring `Waiting`), pushing nothing at
      all on either server;
    * no socket may be closed, since an empty capture on a dead socket reads
      exactly like "the server pushed nothing".
    """
    import datetime
    import threading

    h, j = pairs["h"], pairs["j"]
    legs, notes, problems = {}, {}, {}

    def now_iso():
        return datetime.datetime.now(datetime.timezone.utc).isoformat().replace("+00:00", "Z")

    def group_state(p):
        _, body = p["srv"].http("GET", f"/SyncPlay/{gids[p['srv'].tag]}", p["ctrl"])
        return body.get("State") if isinstance(body, dict) else f"<{body!r}>"

    def collect_all():
        """`collect()` all four sockets CONCURRENTLY.

        Each `collect` waits out its own quiet window, and a socket that receives
        nothing waits the full timeout before it can say so. Run in series that
        is four timeouts per leg, and the run cost scales with the number of legs
        times the number of sockets — which is what made a probe with real state
        coverage look unaffordable. Each `WS` guards its own buffer, and a
        `collect` only reads and drains that one socket, so running them in
        parallel changes nothing about what is compared.
        """
        out, threads = {}, []
        for t in ("h", "j"):
            for which in ("ws_ctrl", "ws_peer"):
                def grab(t=t, which=which):
                    out[(t, which)] = sift(
                        pairs[t][which].collect(),
                        f"{t}/{'ctrl' if which == 'ws_ctrl' else 'peer'}", residue)
                th = threading.Thread(target=grab)
                th.start()
                threads.append(th)
        for th in threads:
            th.join()
        return {t: (out[(t, "ws_ctrl")], out[(t, "ws_peer")]) for t in ("h", "j")}

    def leg(op, label, sess, path, body=None, bodies=None):
        """One compared leg of `op`, issued from the `sess` session of each server.

        `bodies` is a per-server dict when the request has to carry an id the two
        instances minted independently (a `PlaylistItemId`); `body` is the same
        payload for both. Nothing here drives the two servers to "the same" state
        separately — the state they are ALREADY in is read from both and a
        disagreement is a finding, not something to correct for.
        """
        settle(allsock, residue)
        legs.setdefault(op, [])
        notes.setdefault(op, [])
        problems.setdefault(op, [])
        # A dead socket collects nothing, and "nothing" is the same shape as
        # "the server pushed nothing" — so it would quietly turn every remaining
        # leg into a false agreement. Say so instead.
        dead = [lbl for lbl, ws in allsock if ws.closed]
        if dead:
            problems[op].append(
                f"{label}: socket(s) {dead} are CLOSED — this leg was not measured, "
                f"and an empty capture on a dead socket is not an agreement")
        before = {t: group_state(pairs[t]) for t in ("h", "j")}
        if before["h"] != before["j"]:
            problems[op].append(
                f"{label}: the two groups were NOT in the same state before the verb "
                f"— H={before['h']} J={before['j']} (so this leg is not a like-for-like "
                f"comparison; the op that put them out of step is the defect)")
        codes = {}
        for t in ("h", "j"):
            codes[t], _ = pairs[t]["srv"].http(
                "POST", path, pairs[t][sess], bodies[t] if bodies else body)
        got = collect_all()
        if codes["h"] != codes["j"] or codes["h"] != 204:
            problems[op].append(f"{label}: HTTP H={codes['h']} J={codes['j']} (both must be 204)")
        after = {t: group_state(pairs[t]) for t in ("h", "j")}
        if after["h"] != after["j"]:
            problems[op].append(
                f"{label}: the group State AFTER the verb differs — H={after['h']} J={after['j']}")
        notes[op].append(f"{label} {before['j']}→{after['j']}")
        legs[op].append((f"{label}/ctrl", got["j"][0], got["h"][0], subs["j"], subs["h"]))
        legs[op].append((f"{label}/peer", got["j"][1], got["h"][1], subs["j"], subs["h"]))
        return got

    PAUSE, UNPAUSE = "POST /SyncPlay/Pause", "POST /SyncPlay/Unpause"
    STOP, SEEK = "POST /SyncPlay/Stop", "POST /SyncPlay/Seek"
    BUFFER, READY = "POST /SyncPlay/Buffering", "POST /SyncPlay/Ready"
    IGNORE, QUEUE = "POST /SyncPlay/SetIgnoreWait", "POST /SyncPlay/SetNewQueue"
    SETITEM = "POST /SyncPlay/SetPlaylistItem"
    NEXT, PREV = "POST /SyncPlay/NextItem", "POST /SyncPlay/PreviousItem"
    ENQUEUE, REMOVE = "POST /SyncPlay/Queue", "POST /SyncPlay/RemoveFromPlaylist"
    MOVE = "POST /SyncPlay/MovePlaylistItem"
    ALL_OPS = (PAUSE, UNPAUSE, STOP, SEEK, BUFFER, READY, IGNORE, QUEUE,
               SETITEM, NEXT, PREV, ENQUEUE, REMOVE, MOVE)

    def ready_body(plid, position=0, playing=True):
        return {"When": now_iso(), "PositionTicks": position,
                "IsPlaying": playing, "PlaylistItemId": plid}

    def abort(reason):
        """Record `reason` on every verb and return: no leg of any of them ran."""
        for op in ALL_OPS:
            problems.setdefault(op, []).append(reason)
            legs.setdefault(op, [])
            notes.setdefault(op, [])
        return legs, notes, problems

    # -- the queue the rest of the run needs -------------------------------
    # Resolved BEFORE the first leg: an idle group with an empty queue answers
    # most verbs with an all-zero command, which compares almost nothing, and
    # `Unpause` on it pushes a play-queue update neither server sends for an
    # empty playlist. With a real item every leg below carries a playing item.
    # THREE items, because a NextItem/PreviousItem probe needs somewhere to step
    # and a "no room left" edge to fall off.
    catalogue = {}
    for t in ("h", "j"):
        _, body = pairs[t]["srv"].http(
            "GET", "/Items?IncludeItemTypes=Movie&Recursive=true&Limit=1000&SortBy=SortName",
            pairs[t]["ctrl"])
        catalogue[t] = {i.get("Name"): i.get("Id")
                        for i in (body or {}).get("Items", []) if i.get("Id")}
    # The INTERSECTION by name, lowest first. Neither server's ordering is
    # trusted (they disagree — a different row), and neither server's catalogue
    # is assumed to be the other's (this lab's Ferrofin lists 500 movies where
    # its Jellyfin lists 515). Picking from what BOTH can see is what makes the
    # legs below a like-for-like comparison instead of two different files.
    shared = sorted(set(catalogue["h"]) & set(catalogue["j"]))
    if len(shared) < 4:
        return abort(
            f"fewer than four movies are visible on BOTH servers (H has "
            f"{len(catalogue['h'])}, J has {len(catalogue['j'])}, shared "
            f"{len(shared)}) — NO leg of this verb ran")
    picks = shared[:4]
    ids = {t: [catalogue[t][name] for name in picks] for t in ("h", "j")}
    mismatched = [n for k, n in enumerate(picks) if ids["h"][k] != ids["j"][k]]
    if mismatched:
        # Item ids are derived from the path, so the same file is the same id on
        # both. A mismatch means the two libraries are not the same fixture.
        problems.setdefault(QUEUE, []).append(
            f"{mismatched!r} have different ids on the two servers")

    def new_queue(n=3, position=0):
        """A `SetNewQueue` body per server: the first `n` shared movies."""
        return {t: {"PlayingQueue": ids[t][:n], "PlayingItemPosition": position,
                    "StartPositionTicks": 0} for t in ("h", "j")}

    # -- the per-server view of the queue, refreshed from the servers' own pushes
    plid = {"h": None, "j": None}       # the PLAYING item's PlaylistItemId
    playlist = {"h": [], "j": []}       # every PlaylistItemId, in queue order

    def read_queue(msgs):
        """`(playing_item_id, [playlist item ids])` off a `PlayQueue` push — the
        same place a real client reads them. `None` when this leg pushed none."""
        for m in msgs:
            data = (m.get("Data") or {}).get("Data")
            if isinstance(data, dict) and isinstance(data.get("Playlist"), list):
                items = [i.get("PlaylistItemId") for i in data["Playlist"]]
                idx = data.get("PlayingItemIndex")
                current = items[idx] if isinstance(idx, int) and 0 <= idx < len(items) else None
                return current, items
        return None

    def refresh(got, op, label):
        """Re-read the queue from what each server just pushed.

        Every id below is per-instance, so it has to come from the server that
        minted it; a stale one turns a later leg into the "wrong playlist item"
        arm by accident. A leg that was supposed to push a queue update and did
        not on ONE server is recorded as a problem rather than silently reusing
        the old ids.
        """
        seen = {}
        for t in ("h", "j"):
            found = read_queue(got[t][0]) or read_queue(got[t][1])
            seen[t] = found is not None
            if found:
                plid[t], playlist[t] = found
        if seen["h"] != seen["j"]:
            problems.setdefault(op, []).append(
                f"{label}: only one server pushed a PlayQueue update "
                f"(H={seen['h']} J={seen['j']}), so the queue ids are now out of step")

    def per_server(maker):
        """A body built from each server's OWN playlist item id."""
        return {t: maker(plid[t]) for t in ("h", "j")}

    def other_than_playing(t):
        """Any playlist item of `t`'s queue that is not the playing one."""
        return next((i for i in playlist[t] if i and i != plid[t]), None)

    wrong = str(uuid.uuid4())

    # ======================================================================
    # A — what an IDLE group answers.
    # `IdleGroupState` answers Pause/Stop/Seek/Buffering/Ready with a Stop to the
    # CALLER alone (`prevState == Type`, IdleGroupState.cs:113-125) and changes
    # nothing at all — no `Waiting`, no state update.
    # ======================================================================
    got = leg(QUEUE, "setnewqueue@idle", "ctrl", "/SyncPlay/SetNewQueue", bodies=new_queue())
    refresh(got, QUEUE, "setnewqueue@idle")
    if not plid["h"] or not plid["j"]:
        return abort(f"no PlaylistItemId in the PlayQueue push (H={plid['h']} "
                     f"J={plid['j']}) — NO leg of this verb ran")

    leg(STOP, "stop@waiting", "ctrl", "/SyncPlay/Stop")
    leg(PAUSE, "pause@idle", "ctrl", "/SyncPlay/Pause")
    leg(SEEK, "seek@idle", "ctrl", "/SyncPlay/Seek", {"PositionTicks": 5_000_000})
    leg(BUFFER, "buffer@idle", "ctrl", "/SyncPlay/Buffering",
        bodies=per_server(lambda p: ready_body(p, playing=False)))
    leg(READY, "ready@idle", "ctrl", "/SyncPlay/Ready",
        bodies=per_server(lambda p: ready_body(p, playing=False)))
    # `AbstractGroupState.cs:207-211` — outside `Waiting` this records the flag
    # and nothing else, so neither server may push anything.
    leg(IGNORE, "ignorewait@idle", "ctrl", "/SyncPlay/SetIgnoreWait", {"IgnoreWait": False})
    leg(STOP, "stop@idle", "ctrl", "/SyncPlay/Stop")

    # ======================================================================
    # B — the queue verbs. One arm each on `AbstractGroupState`, but several
    # branches inside it, and the branches are what this block drives.
    # ======================================================================
    # :114-132 — `Group.AddToPlayQueue` succeeded: AllGroup queue update.
    got = leg(ENQUEUE, "queue@idle", "ctrl", "/SyncPlay/Queue",
              bodies={t: {"ItemIds": [ids[t][3]], "Mode": "Queue"} for t in ("h", "j")})
    refresh(got, ENQUEUE, "queue@idle")
    # :118-122 — `AddToPlayQueue` returns FALSE on an empty list (Group.cs:575-579),
    # and the arm then broadcasts NOTHING. Ferrofin used to push the update anyway.
    leg(ENQUEUE, "queue@idle/nothing", "ctrl", "/SyncPlay/Queue",
        {"ItemIds": [], "Mode": "Queue"})
    # :97-112 — a move, which re-anchors `PlayingItemIndex` on the item that was
    # playing (`PlayingItemIndex = playlist.IndexOf(playingItem)`).
    got = leg(MOVE, "move@idle", "ctrl", "/SyncPlay/MovePlaylistItem",
              bodies={t: {"PlaylistItemId": playlist[t][-1], "NewIndex": 0}
                      for t in ("h", "j")})
    refresh(got, MOVE, "move@idle")
    # :69-95 — `playingItemRemoved` is FALSE here, so no Stop: the queue shrank
    # and the group carries on.
    got = leg(REMOVE, "remove@idle/keeps-playing", "ctrl", "/SyncPlay/RemoveFromPlaylist",
              bodies={t: {"PlaylistItemIds": [other_than_playing(t)],
                          "ClearPlaylist": False, "ClearPlayingItem": False}
                      for t in ("h", "j")})
    refresh(got, REMOVE, "remove@idle/keeps-playing")
    # ...and `ClearPlaylist` with `ClearPlayingItem=false` KEEPS the playing item
    # (PlayQueueManager.cs:176-197), so again no Stop. Ferrofin used to ignore
    # `ClearPlayingItem` entirely, wipe the queue and Stop the group.
    got = leg(REMOVE, "remove@idle/clear-keeps-playing-item", "ctrl",
              "/SyncPlay/RemoveFromPlaylist",
              {"PlaylistItemIds": [], "ClearPlaylist": True, "ClearPlayingItem": False})
    refresh(got, REMOVE, "remove@idle/clear-keeps-playing-item")
    # ...and only THIS empties the queue, which is the one case that Stops.
    leg(REMOVE, "remove@idle/clear-everything", "ctrl", "/SyncPlay/RemoveFromPlaylist",
        {"PlaylistItemIds": [], "ClearPlaylist": True, "ClearPlayingItem": True})

    # ======================================================================
    # C — the queue-STEP verbs, and the arm every one of them falls into when
    # the change cannot be applied. That arm is `prevState switch { Playing =>
    # Playing, Paused => Paused, _ => Idle }` (WaitingGroupState.cs:144-148,
    # :189-194, :595-600, :641-646): a group that was ALREADY `Waiting` drops to
    # `Idle`. It pushes NOTHING, so only the state assertion can see it — which
    # is why Ferrofin sat in `Waiting` here undetected until a reviewer measured
    # it by hand.
    # ======================================================================
    got = leg(QUEUE, "setnewqueue@idle/steps", "ctrl", "/SyncPlay/SetNewQueue",
              bodies=new_queue(3, 1))
    refresh(got, QUEUE, "setnewqueue@idle/steps")
    # WaitingGroupState.cs:575-579 — a step naming an item that is not the
    # playing one is a duplicate request: dropped, no push, state unchanged.
    leg(NEXT, "nextitem@waiting/wrong-item", "ctrl", "/SyncPlay/NextItem",
        {"PlaylistItemId": wrong})
    got = leg(NEXT, "nextitem@waiting", "ctrl", "/SyncPlay/NextItem",
              bodies=per_server(lambda p: {"PlaylistItemId": p}))
    refresh(got, NEXT, "nextitem@waiting")
    # ...and now there is no next item: nothing is pushed and the group falls to Idle.
    leg(NEXT, "nextitem@waiting/no-room", "ctrl", "/SyncPlay/NextItem",
        bodies=per_server(lambda p: {"PlaylistItemId": p}))
    got = leg(PREV, "previtem@idle", "ctrl", "/SyncPlay/PreviousItem",
              bodies=per_server(lambda p: {"PlaylistItemId": p}))
    refresh(got, PREV, "previtem@idle")
    got = leg(PREV, "previtem@waiting", "ctrl", "/SyncPlay/PreviousItem",
              bodies=per_server(lambda p: {"PlaylistItemId": p}))
    refresh(got, PREV, "previtem@waiting")
    leg(PREV, "previtem@waiting/no-room", "ctrl", "/SyncPlay/PreviousItem",
        bodies=per_server(lambda p: {"PlaylistItemId": p}))
    got = leg(SETITEM, "setplaylistitem@idle", "ctrl", "/SyncPlay/SetPlaylistItem",
              bodies={t: {"PlaylistItemId": playlist[t][-1]} for t in ("h", "j")})
    refresh(got, SETITEM, "setplaylistitem@idle")
    leg(SETITEM, "setplaylistitem@waiting/unknown", "ctrl", "/SyncPlay/SetPlaylistItem",
        {"PlaylistItemId": wrong})

    # ======================================================================
    # D — WAITING, resolved by Ready.
    # ======================================================================
    got = leg(QUEUE, "setnewqueue@idle/play", "ctrl", "/SyncPlay/SetNewQueue",
              bodies=new_queue())
    refresh(got, QUEUE, "setnewqueue@idle/play")
    # :407-418 — a Ready naming the wrong item is not a Ready: the caller alone
    # is sent the queue and the group keeps waiting.
    leg(READY, "ready@waiting/wrong-item", "peer", "/SyncPlay/Ready", ready_body(wrong))
    # :380-391, the THIRD Buffer arm, with the CORRECT item: "another session is
    # now buffering". `ResumePlaying` is armed here (the group entered Waiting
    # through a Play), so the arm sends the state update and NO command.
    leg(BUFFER, "buffer@waiting/resume-armed", "peer", "/SyncPlay/Buffering",
        bodies=per_server(lambda p: ready_body(p, playing=False)))
    # :471-479 — one of two members ready: it alone is told to pause when it
    # reaches the group's position, and the group stays Waiting.
    leg(READY, "ready@waiting/first", "ctrl", "/SyncPlay/Ready", bodies=per_server(ready_body))
    # :484-517 — the last Ready starts playback for everyone.
    leg(READY, "ready@waiting/last", "peer", "/SyncPlay/Ready", bodies=per_server(ready_body))

    # ======================================================================
    # E — the PLAYING arms.
    # ======================================================================
    # `PlayingGroupState.cs:81-86` / `:133-138`: "client got lost" — the caller
    # alone is resynced, and the group's clock is NOT moved for everybody else.
    leg(UNPAUSE, "unpause@playing", "ctrl", "/SyncPlay/Unpause")
    leg(READY, "ready@playing", "ctrl", "/SyncPlay/Ready", bodies=per_server(ready_body))
    # :346-368 — a Buffer out of Playing pauses `AllReady`, which EXCLUDES the
    # member that just said it is buffering: the peer issues it, so only the
    # controller is told. With one member in the group this is invisible.
    leg(BUFFER, "buffer@playing", "peer", "/SyncPlay/Buffering",
        bodies=per_server(lambda p: ready_body(p, playing=False)))
    # :333-344 — the wrong item: the caller gets its queue back and nothing else.
    leg(BUFFER, "buffer@waiting/wrong-item", "ctrl", "/SyncPlay/Buffering",
        ready_body(wrong, playing=False))

    # -- Seek, including the clamp ----------------------------------------
    leg(SEEK, "seek@waiting", "ctrl", "/SyncPlay/Seek", {"PositionTicks": 5_000_000})
    # `Group.SanitizePositionTicks` clamps to the item's run time (Group.cs:429-432);
    # the fixture movies are about a second long, so this is far past the end of
    # the file and a server without the ceiling echoes the request straight back.
    leg(SEEK, "seek@waiting/past-the-end", "ctrl", "/SyncPlay/Seek",
        {"PositionTicks": 999_999_999})

    # -- the forced Unpause, and the group-wait it turns off ---------------
    # :228-242 — an Unpause while waiting WITH the resume armed starts playback
    # regardless of who is still buffering, and disarms group-wait until the next
    # state change...
    leg(UNPAUSE, "unpause@waiting/forced", "ctrl", "/SyncPlay/Unpause")
    # ...so `PlayingGroupState.cs:117-123` drops the very next Buffer.
    leg(BUFFER, "buffer@playing/ignored", "peer", "/SyncPlay/Buffering",
        bodies=per_server(lambda p: ready_body(p, playing=False)))
    # `PlayingGroupState.cs:88-93` — a Seek out of Playing drops to Waiting with
    # the resume ARMED, so the next Unpause starts playback immediately.
    leg(SEEK, "seek@playing", "ctrl", "/SyncPlay/Seek", {"PositionTicks": 3_000_000})
    leg(UNPAUSE, "unpause@waiting/forced-2", "ctrl", "/SyncPlay/Unpause")

    # ======================================================================
    # F — SetNewQueue from the three states the original probe never drove it
    # from, ending on the arm that pushes nothing and only moves the state.
    # ======================================================================
    got = leg(QUEUE, "setnewqueue@playing", "ctrl", "/SyncPlay/SetNewQueue",
              bodies=new_queue())
    refresh(got, QUEUE, "setnewqueue@playing")
    got = leg(QUEUE, "setnewqueue@waiting", "ctrl", "/SyncPlay/SetNewQueue",
              bodies=new_queue())
    refresh(got, QUEUE, "setnewqueue@waiting")
    leg(UNPAUSE, "unpause@waiting/forced-3", "ctrl", "/SyncPlay/Unpause")
    leg(PAUSE, "pause@playing", "ctrl", "/SyncPlay/Pause")
    got = leg(QUEUE, "setnewqueue@paused", "ctrl", "/SyncPlay/SetNewQueue",
              bodies=new_queue())
    refresh(got, QUEUE, "setnewqueue@paused")
    # THE arm the reviewer measured by hand: `Group.SetPlayQueue` refuses an empty
    # queue (Group.cs:492-495), so nothing is pushed and the group takes the
    # `prevState switch` default — `Idle`, not back to `Waiting`.
    leg(QUEUE, "setnewqueue@waiting/refused", "ctrl", "/SyncPlay/SetNewQueue",
        {"PlayingQueue": [], "PlayingItemPosition": 0, "StartPositionTicks": 0})

    # ======================================================================
    # G — the PAUSED arms and the states left over.
    #
    # The ORDER here is load-bearing, not decorative. Two arms are only
    # reachable from a particular buffering set, and driving them out of order
    # silently turns them into a different arm that pushes nothing:
    #
    #   * `WaitingGroupState.cs:655-678` (SetIgnoreWait) only RELEASES the group
    #     when the caller is the LAST member still buffering. `Seek` while
    #     waiting calls `SetAllBuffering(true)`, so a Seek anywhere before it
    #     leaves the peer buffering too and the release never happens — the row
    #     then compares two silences and reports "nothing compared". So the
    #     group is put into `Waiting` for that leg by a `Buffer` from the
    #     CONTROLLER (`PausedGroupState.cs:106-111` -> `SetBuffering(session)`,
    #     one member only), never by a Seek.
    #   * the `!ResumePlaying` half of the third Buffer arm needs the resume
    #     DISARMED, which is what entering `Waiting` from `Paused` does.
    # ======================================================================
    # IdleGroupState.cs:57-63 -> WaitingGroupState.cs:212-225: an idle group
    # RESTARTS the current item and waits — it does not stop.
    got = leg(UNPAUSE, "unpause@idle", "ctrl", "/SyncPlay/Unpause")
    refresh(got, UNPAUSE, "unpause@idle")
    leg(READY, "ready@waiting/first-2", "ctrl", "/SyncPlay/Ready", bodies=per_server(ready_body))
    leg(READY, "ready@waiting/last-2", "peer", "/SyncPlay/Ready", bodies=per_server(ready_body))
    leg(PAUSE, "pause@playing/again", "ctrl", "/SyncPlay/Pause")
    leg(PAUSE, "pause@paused", "ctrl", "/SyncPlay/Pause")
    leg(READY, "ready@paused", "ctrl", "/SyncPlay/Ready",
        bodies=per_server(lambda p: ready_body(p, playing=False)))
    leg(IGNORE, "ignorewait@paused", "ctrl", "/SyncPlay/SetIgnoreWait", {"IgnoreWait": False})
    # `PausedGroupState.cs:100-105` — a Seek out of Paused drops to Waiting with
    # the resume DISARMED, so the next Unpause only ARMS it (:243-250) and a
    # second one is needed to actually start playback.
    leg(SEEK, "seek@paused", "ctrl", "/SyncPlay/Seek", {"PositionTicks": 2_000_000})
    # :243-250 — arms the resume, no command, still Waiting.
    leg(UNPAUSE, "unpause@waiting/arms-resume", "ctrl", "/SyncPlay/Unpause")
    leg(UNPAUSE, "unpause@waiting/forced-4", "ctrl", "/SyncPlay/Unpause")
    leg(PAUSE, "pause@playing/third", "ctrl", "/SyncPlay/Pause")
    leg(UNPAUSE, "unpause@paused", "ctrl", "/SyncPlay/Unpause")
    leg(PAUSE, "pause@playing/fourth", "ctrl", "/SyncPlay/Pause")
    # `PausedGroupState.cs:94-99` — a Stop out of Paused reaches the whole group
    # (`prevState != Idle`), unlike the Idle arm's caller-only Stop.
    leg(STOP, "stop@paused", "ctrl", "/SyncPlay/Stop")

    # -- the last Waiting arms, entered from Paused so exactly ONE member buffers
    got = leg(UNPAUSE, "unpause@idle/again", "ctrl", "/SyncPlay/Unpause")
    refresh(got, UNPAUSE, "unpause@idle/again")
    leg(READY, "ready@waiting/first-3", "ctrl", "/SyncPlay/Ready", bodies=per_server(ready_body))
    leg(READY, "ready@waiting/last-3", "peer", "/SyncPlay/Ready", bodies=per_server(ready_body))
    leg(PAUSE, "pause@playing/fifth", "ctrl", "/SyncPlay/Pause")
    # :369-379 — a Buffer out of Paused pauses the CALLER only and drops the
    # group into Waiting with the resume DISARMED. Only the caller is marked
    # buffering, which is the precondition the SetIgnoreWait release needs.
    leg(BUFFER, "buffer@paused", "ctrl", "/SyncPlay/Buffering",
        bodies=per_server(lambda p: ready_body(p, playing=False)))
    # :255-269 — stays Waiting, keeps the resume disarmed, state update only.
    leg(PAUSE, "pause@waiting", "ctrl", "/SyncPlay/Pause")
    # :522-535 — the group is settling into Paused, so a Ready from a client that
    # is nowhere near the group's position is corrected instead of accepted.
    leg(READY, "ready@waiting/correcting", "ctrl", "/SyncPlay/Ready",
        bodies=per_server(lambda p: ready_body(p, playing=False)))
    # :380-391 again, the OTHER half of the third Buffer arm: with the resume
    # DISARMED the newly-buffering caller is force-updated with a Pause.
    leg(BUFFER, "buffer@waiting/resume-disarmed", "ctrl", "/SyncPlay/Buffering",
        bodies=per_server(lambda p: ready_body(p, playing=False)))
    # :243-250 — arms the resume, still no command, still Waiting.
    leg(UNPAUSE, "unpause@waiting/arms-resume-2", "ctrl", "/SyncPlay/Unpause")
    # :655-678 — the member the group was waiting for asks not to be waited for,
    # which is what RELEASES the group (`Group.IsBuffering` skips it). The peer
    # has been ready since `ready@waiting/last-3`, so the caller is the last one.
    leg(IGNORE, "ignorewait@waiting", "ctrl", "/SyncPlay/SetIgnoreWait", {"IgnoreWait": True})
    leg(IGNORE, "ignorewait@playing", "ctrl", "/SyncPlay/SetIgnoreWait", {"IgnoreWait": False})

    # -- back to Idle so the GET/{id} and Leave legs start where they did ---
    leg(STOP, "stop@playing/teardown", "ctrl", "/SyncPlay/Stop")
    return legs, notes, problems


# ---------------------------------------------------------------- the run

def run(ferrofin_url, jellyfin_url):
    rows, errors, residue = {}, [], []
    proven = frozenset()
    observations_pre = []
    H, J = Server(ferrofin_url, "h"), Server(jellyfin_url, "j")
    stamp = uuid.uuid4().hex[:8]
    created = []            # (server, admin-session, user-id) to reap in `finally`
    try:
        pairs = {}
        for srv in (H, J):
            ctrl = srv.login(f"push-{stamp}-a")
            # The joiner is a DIFFERENT user, so `Participants` has two names and
            # its ORDER is a real comparison (see JOINER_USER).
            created.append((srv, ctrl, srv.ensure_user(ctrl, JOINER_USER, JOINER_PASS)))
            peer = srv.login(f"push-{stamp}-b", JOINER_USER, JOINER_PASS)
            pairs[srv.tag] = {"srv": srv, "ctrl": ctrl, "peer": peer,
                              "ws_ctrl": srv.socket(ctrl), "ws_peer": srv.socket(peer)}
        h, j = pairs["h"], pairs["j"]
        allsock = [("h/ctrl", h["ws_ctrl"]), ("h/peer", h["ws_peer"]),
                   ("j/ctrl", j["ws_ctrl"]), ("j/peer", j["ws_peer"])]

        # Earn the right to ignore stray `LibraryChanged` frames, before any row
        # is measured. If either server fails to push one, the name is NOT
        # excluded and this layer's rows withdraw exactly as they did when
        # Ferrofin had no notifier at all.
        ok, detail = prove_library_changed(pairs, allsock, residue)
        if ok:
            proven = frozenset({"LibraryChanged"})
            observations_pre.append("PROVEN THIS RUN — " + detail)
        else:
            errors.append("LibraryChanged proof failed — " + detail)

        # -- the lab precondition GET /SyncPlay/List depends on -----------------
        # A SyncPlay group outlives the session that made it: a probe or a hand
        # diagnosis that exits without a `Leave` leaves one behind, and it stays
        # until that server restarts. Ferrofin's are cleared by any rebuild of its
        # image, Jellyfin's are not — so the residue is ASYMMETRIC by default, and
        # the `GET /SyncPlay/List` row below will read a leftover on one side as a
        # Ferrofin defect. Named here, loudly, before any row is measured, because
        # whoever runs the final sweep cannot debug it from a red diff alone.
        stale = {}
        for p_ in (h, j):
            _, body = p_["srv"].http("GET", "/SyncPlay/List", p_["ctrl"])
            stale[p_["srv"].tag] = [g.get("GroupName") for g in body
                                    if isinstance(g, dict)] if isinstance(body, list) else []
        if stale["h"] or stale["j"]:
            errors.append(
                f"LAB RESIDUE: SyncPlay groups already existed before this run — "
                f"H={stale['h']} J={stale['j']}. They are left over from an earlier "
                f"probe or diagnosis that did not Leave, they survive until that "
                f"server restarts, and they make GET /SyncPlay/List diff red for a "
                f"reason that is not a Ferrofin defect. Clear them by ending the "
                f"sessions that hold them (DELETE /Devices?id=… logs a device out, "
                f"which fires SessionEnded -> LeaveGroup) before trusting that row.")

        # -- POST /SyncPlay/Ping from a session that is in NO group ------------
        # The one playback verb upstream does NOT gate on `SyncPlayIsInGroup`
        # (`SyncPlayController.SyncPlayPing` carries no route policy), so it must
        # be accepted and answered with a `NotInGroup` push rather than a 403.
        # Run FIRST, while neither session has ever joined anything.
        settle(allsock, residue)
        st_h, _ = h["srv"].http("POST", "/SyncPlay/Ping", h["ctrl"], {"Ping": 77})
        st_j, _ = j["srv"].http("POST", "/SyncPlay/Ping", j["ctrl"], {"Ping": 77})
        pings = (sift(h["ws_ctrl"].collect(), "h/ctrl", residue),
                 sift(j["ws_ctrl"].collect(), "j/ctrl", residue))
        ok, compared, note = compare_pushes(
            [("ping/controller", pings[1], pings[0], {}, {})])
        ok = ok and st_h == st_j
        rows["POST /SyncPlay/Ping"] = verdict(
            ok, compared, f"H={st_h} J={st_j} | {note} | {compared} field(s) compared (pushed messages)",
            extra="the not-in-group leg: accepted (no IsInGroup policy) + a NotInGroup push")

        # -- POST /SyncPlay/New ------------------------------------------------
        settle(allsock, residue)
        new_bodies, gids = {}, {}
        for p in (h, j):
            st, body = p["srv"].http("POST", "/SyncPlay/New", p["ctrl"],
                                     {"GroupName": "  parity push  "})
            new_bodies[p["srv"].tag] = (st, body)
            gids[p["srv"].tag] = body.get("GroupId") if isinstance(body, dict) else None
        pushes = {t: sift(pairs[t]["ws_ctrl"].collect(), f"{t}/ctrl", residue)
                  for t in ("h", "j")}
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
            # Aborts through the `except` so the finally-teardown and the
            # settle-window residue report still run — a bare `return` here would
            # skip both and leave a user behind on each server.
            raise RuntimeError("SyncPlay/New did not return a GroupId on both servers")

        # -- POST /SyncPlay/Join ----------------------------------------------
        # Two sockets matter: the joiner (GroupJoined + the state hook) and the
        # member already in the group (UserJoined). Both are compared.
        settle(allsock, residue)
        st = {}
        for p, gid in ((h, gids["h"]), (j, gids["j"])):
            st[p["srv"].tag], _ = p["srv"].http("POST", "/SyncPlay/Join", p["peer"],
                                                {"GroupId": gid})
        jn = {t: (sift(pairs[t]["ws_peer"].collect(), f"{t}/peer", residue),
                  sift(pairs[t]["ws_ctrl"].collect(), f"{t}/ctrl", residue))
              for t in ("h", "j")}
        ok, compared, note = compare_pushes([
            ("join/joiner", jn["j"][0], jn["h"][0], sj, sh),
            ("join/member", jn["j"][1], jn["h"][1], sj, sh),
        ])
        rows["POST /SyncPlay/Join"] = verdict(
            ok and st["h"] == st["j"], compared,
            f"H={st['h']} J={st['j']} | {note} | {compared} field(s) compared (pushed messages)")

        # -- the playback verbs, every state ----------------------------------
        # One row per verb, whose verdict is the AND of every state's leg. See
        # `playback_commands` for why each verb is driven from all four states
        # and why both sockets are compared.
        legs, pb_notes, pb_problems = playback_commands(
            pairs, gids, {"h": sh, "j": sj}, allsock, residue, rows)
        for op in sorted(legs):
            ok, compared, note = compare_pushes(legs[op])
            ok = ok and not pb_problems.get(op)
            full = "; ".join(pb_notes.get(op, [])) + f" | {note}"
            if pb_problems.get(op):
                full += " | " + " | ".join(pb_problems[op])
            rows[op] = verdict(
                ok, compared,
                f"{full} | {compared} field(s) compared (pushed messages)")
        for op, probs in sorted(pb_problems.items()):
            if op not in rows:
                # A verb whose legs never ran (an unresolvable fixture) must not
                # silently vanish from the results file.
                rows[op] = verdict(False, 0, " | ".join(probs))

        # -- GET /SyncPlay/List ------------------------------------------------
        # A real JSON body, so this row is body-diff, not push-diff. It is read
        # HERE, while the group this layer made is the only one either server has
        # — which is exactly what makes it a check on the lab as well as on the
        # handler: a leftover group on one side shows up as an extra element.
        settle(allsock, residue, 0.3)
        lst = {}
        for p_, gid in ((h, gids["h"]), (j, gids["j"])):
            lst[p_["srv"].tag] = p_["srv"].http("GET", "/SyncPlay/List", p_["ctrl"])
        n, c, paths = diff_docs(lst["j"][1], lst["h"][1], sj, sh)
        counts = {t: len(b) if isinstance(b, list) else -1 for t, (_, b) in lst.items()}
        rows["GET /SyncPlay/List"] = verdict(
            n == 0 and lst["h"][0] == lst["j"][0] == 200 and counts["h"] == counts["j"] == 1,
            c,
            f"H={lst['h'][0]} J={lst['j'][0]} | groups H={counts['h']} J={counts['j']} "
            f"(one each: the group this run made) | {c} field(s) compared"
            + (f", {n} diff(s) at {paths[:4]}" if n else ""),
            method=verification.BODY_DIFF)

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
        settle(allsock, residue, 0.3)
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
        settle(allsock, residue)
        st = {}
        for p in (h, j):
            st[p["srv"].tag], _ = p["srv"].http("POST", "/SyncPlay/Leave", p["peer"])
        lv = {t: (sift(pairs[t]["ws_peer"].collect(), f"{t}/peer", residue),
                  sift(pairs[t]["ws_ctrl"].collect(), f"{t}/ctrl", residue))
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
        # Symmetric teardown: the joiner account this layer created is removed on
        # BOTH servers, so the pair is left exactly as it was found.
        for srv, admin, uid in created:
            try:
                srv.http("DELETE", f"/Users/{uid}", admin)
            except Exception as e:                           # noqa: BLE001
                errors.append(f"could not delete {JOINER_USER} on {srv.base}: {e}")
    res_errors, observations = residue_report(residue, proven)
    observations = observations_pre + observations
    withdraw_on_incomplete(rows, res_errors)
    return rows, errors + res_errors, observations


def stamp_age(body, asked):
    """Seconds between `LastUpdatedAt` and when the read was issued, or None."""
    if not isinstance(body, dict):
        return None
    t = parse_time(body.get("LastUpdatedAt"))
    return None if t is None else abs(t - asked)


def fmt_age(age):
    return "n/a" if age is None else f"{age:.1f}s"


def verdict(ok, compared, note, method=None, extra=None):
    """One results row. Three outcomes, weighed in THIS order:

    1. A DEFINITE FAILURE outranks everything else. When the probe SAW a
       difference — a pushed message sequence that does not match, differing HTTP
       statuses, a payload field or a `When` offset that diffs — the row is RED,
       even when the mismatch left nothing to compare field-by-field. That case
       is not hypothetical and it is not rare: `compare_pushes` cannot diff the
       payloads of a message one server never pushed, so the real defect
       `H=[] J=['NotInGroup']` yields `compared == 0`. Filing it as "the probe
       compared no fields" would make a genuine SERVER defect read as a HARNESS
       shortfall in LEDGER.md, which renders the classification, not the note.
    2. Nothing compared AND nothing wrong is UNTESTED — no verdict, no method. A
       probe that measured nothing must not claim a result.
    3. A clean comparison earns its method.
    """
    full = note + (f" ({extra})" if extra else "")
    if not ok:
        return {"deep_verified": False,
                "verification_method": method or verification.PUSH_DIFF,
                "note": full,
                "classification":
                    "flagged: pushed messages or response differ (verify against the C#)"}
    if not compared:
        return {"deep_verified": None, "verification_method": None,
                "note": f"nothing compared — {note}",
                "classification": "flagged: the push probe compared no fields"}
    return {"deep_verified": True,
            "verification_method": method or verification.PUSH_DIFF,
            "note": full, "classification": "ok"}


def main():
    if "--check" in sys.argv:
        selfcheck()
        return
    rows, errors, observations = run(
        os.environ.get("FERROFIN_URL", "http://localhost:18096"),
        os.environ.get("JELLYFIN_URL", "http://localhost:18097"))
    out = {"generated_by": "suite/parity/push.py",
           "last_verified": os.environ.get("PARITY_STAMP", ""),
           "errors": errors,
           # What the settle windows drained. Recorded rather than swallowed, so a
           # divergence outside any row is visible instead of laundered.
           "observations": observations,
           "rows": rows}
    with open(os.path.join(ROOT, "suite/parity/push-results.json"), "w") as f:
        json.dump(out, f, indent=2, sort_keys=True)
        f.write("\n")
    ok = sum(1 for v in rows.values() if v["deep_verified"])
    print(f"wrote parity/push-results.json — {len(rows)} op(s), {ok} verified")
    for e in errors:
        print(f"  !! {e}", file=sys.stderr)
    for k, v in sorted(rows.items()):
        print(f"  {v['deep_verified']!s:>5} {v['verification_method'] or '-':<10} {k}: {v['note']}")
    for o in observations:
        print(f"  settle-window residue: {o}")


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

    # 2b. ...and a sequence mismatch is a RED, not an "untested". A message one
    #     server never pushed has no payload to diff, so `compared` can be 0 while
    #     the finding is definite; `verdict` must weigh the failure first, or a
    #     server defect renders in LEDGER.md as a harness shortfall.
    ok0, n0, note0 = compare_pushes([("x", [gj(GJ)], [], sj, sh)])
    assert not ok0 and n0 == 0, (ok0, n0)
    row = verdict(ok0, n0, note0)
    assert row["deep_verified"] is False, row
    assert row["verification_method"] == verification.PUSH_DIFF, row
    assert "differ" in row["classification"], row

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

    # 10. `When` is denylisted, so it is asserted through its DERIVED offset
    #     instead. Two servers whose commands are stamped from LastActivity agree
    #     on `When − EmittedAt`; one that stamped `When = now` reports ~0 and is
    #     caught — which the field-by-field diff, skipping both fields, cannot do.
    def cmd(gid, when, emitted):
        m = stop(gid, when)
        m["Data"]["EmittedAt"] = emitted
        return m

    late_j = cmd(GJ, "2026-01-01T00:00:00.000000Z", "2026-01-01T00:00:03.000000Z")
    late_h = cmd(GH, "2026-01-01T00:10:00.000000Z", "2026-01-01T00:10:03.020000Z")
    ok, n, note = compare_pushes([("x", [late_j], [late_h], sj, sh)])
    assert ok and "When−EmittedAt" in note, note        # -3.00s vs -2.98s
    now_h = cmd(GH, "2026-01-01T00:10:03.000000Z", "2026-01-01T00:10:03.000000Z")
    ok, _, note = compare_pushes([("x", [late_j], [now_h], sj, sh)])
    assert not ok and "offset differs" in note, note
    # ...and one server dropping the pair entirely is a difference, not a skip.
    ok, _, note = compare_pushes([("x", [late_j], [stop(GH, "t")], sj, sh)])
    assert not ok and "only one server carries" in note, note

    # 11. the settle windows REPORT what they drain. A socket-lifecycle frame
    #     both servers send is an agreed observation; one only Jellyfin sends is a
    #     recorded COUNT DELTA (the `ForceKeepAlive` cadence); anything else is an
    #     error, because a late SyncPlay frame means its leg was mismeasured.
    errs, obs = residue_report([("h/ctrl", "ForceKeepAlive"), ("j/ctrl", "ForceKeepAlive")])
    assert not errs and obs and obs[0].startswith("agreed"), (errs, obs)
    errs, obs = residue_report([("j/ctrl", "ForceKeepAlive"), ("j/peer", "ForceKeepAlive")])
    assert not errs and obs and obs[0].startswith("COUNT DELTA"), (errs, obs)
    errs, obs = residue_report([("h/ctrl", "SyncPlayCommand/Stop")])
    assert errs and not obs and "incomplete capture" in errs[0], (errs, obs)
    #     `LibraryChanged` is excluded ONLY when the run proved both servers push
    #     it. Unproven it is an error that withdraws the greens -- which is what
    #     an unported notifier must cost. Proving it must never be a constant.
    unproven, _ = residue_report([("j/ctrl", "LibraryChanged")])
    assert unproven and "incomplete capture" in unproven[0], unproven
    errs2, obs2 = residue_report([("j/ctrl", "LibraryChanged")],
                                 proven=frozenset({"LibraryChanged"}))
    assert not errs2 and obs2 and "BACKGROUND BROADCAST" in obs2[0], (errs2, obs2)
    #     ...and `sift` honours the same proof, so an in-window frame is treated
    #     exactly like a between-legs one rather than compared as a verb's output.
    PROVEN.add("LibraryChanged")
    try:
        assert sift([{"MessageType": "LibraryChanged", "Data": {}}], "h/ctrl", []) == [], \
            "a proven broadcast must leave the compared set wherever it lands"
    finally:
        PROVEN.discard("LibraryChanged")
    assert sift([{"MessageType": "LibraryChanged", "Data": {}}], "h/ctrl", []) != [], \
        "unproven, it must stay in the compared set and be able to fail a row"
    assert "LibraryChanged" not in UNATTRIBUTABLE, (
        "the allowance must be earned per run by prove_library_changed, never "
        "baked into the static set -- a regression that silenced the push would "
        "otherwise look exactly like a quiet run")
    #     ...and a BACKGROUND broadcast (a scheduled task finishing on one server
    #     mid-leg) is taken out of the compared sets by `sift` and COUNTED here,
    #     never attributed to whichever verb was in flight — while a message no
    #     rule names stays in the compared set and can still fail its row.
    res = []
    kept = sift([{"MessageType": "ScheduledTaskEnded", "Data": {}},
                 {"MessageType": "ForceKeepAlive", "Data": 60},
                 {"MessageType": "SyncPlayCommand", "Data": {"Command": "Stop"}},
                 {"MessageType": "GeneralCommand", "Data": {}}], "h/ctrl", res)
    assert [m["MessageType"] for m in kept] == ["SyncPlayCommand", "GeneralCommand"], kept
    assert sorted(k for _, k in res) == ["ForceKeepAlive", "ScheduledTaskEnded"], res
    errs, obs = residue_report(res)
    assert not errs and any(o.startswith("BACKGROUND BROADCAST") for o in obs), (errs, obs)
    #     ...and it must NOT withdraw the run's greens the way a lost SyncPlay
    #     frame does: it was never that leg's output to begin with.
    g = {"a": verdict(True, 9, "n")}
    withdraw_on_incomplete(g, errs)
    assert g["a"]["deep_verified"] is True, g
    errs, _ = residue_report([("h/ctrl", "SyncPlayCommand/Stop")])
    #     ...and an incomplete capture WITHDRAWS the run's greens (a short
    #     capture both servers shared would otherwise agree), while leaving a red
    #     exactly as red — the withdrawal can never manufacture a pass.
    r = {"a": verdict(True, 9, "n"), "b": verdict(False, 9, "n")}
    withdraw_on_incomplete(r, errs)
    assert r["a"]["deep_verified"] is None and r["a"]["verification_method"] is None
    assert "incomplete capture" in r["a"]["note"]
    assert r["b"]["deep_verified"] is False, r["b"]
    clean = {"a": verdict(True, 9, "n")}
    withdraw_on_incomplete(clean, [])
    assert clean["a"]["deep_verified"] is True

    # 12. the freshness property used for GET /SyncPlay/{id}.
    now = time.time()
    import datetime
    iso = datetime.datetime.fromtimestamp(now, datetime.timezone.utc).isoformat()
    assert stamp_age({"LastUpdatedAt": iso.replace("+00:00", "Z")}, now) < 1
    assert stamp_age({"LastUpdatedAt": "2020-01-01T00:00:00.1234567Z"}, now) > 30
    assert stamp_age({}, now) is None

    # 13. a RUNNING group's `PositionTicks` advances with the wall clock, so it is
    #     compared with a tolerance rather than exactly — and the tolerance must
    #     still reject a real difference, a zero on one side, and the missing
    #     Seek clamp this batch measured.
    def at(gid, pos):
        m = stop(gid, "2026-01-01T00:00:00Z")
        m["Data"]["PositionTicks"] = pos
        m["Data"]["Command"] = "Pause"
        return m

    ok, n, note = compare_pushes([("x", [at(GJ, 71_000_000)], [at(GH, 71_080_000)], sj, sh)])
    assert ok and "tolerance" in note and "Δ" in note, note      # 8ms apart
    ok, _, note = compare_pushes([("x", [at(GJ, 71_000_000)], [at(GH, 71_800_000)], sj, sh)])
    assert not ok and "PositionTicks" in note, note              # 80ms — REJECTED at 20ms
    ok, _, note = compare_pushes([("x", [at(GJ, 71_000_000)], [at(GH, 170_000_000)], sj, sh)])
    assert not ok and "PositionTicks" in note, note              # ~10s apart
    ok, _, note = compare_pushes([("x", [at(GJ, 71_000_000)], [at(GH, 0)], sj, sh)])
    assert not ok and "PositionTicks" in note, note              # a zero is never softened
    #     ...and the defect the Seek row exists for: the oracle clamps to the
    #     item's run time, a server without the ceiling echoes the request back.
    ok, _, note = compare_pushes([("x", [at(GJ, 10_230_000)], [at(GH, 999_999_999)], sj, sh)])
    assert not ok and "PositionTicks" in note, note

    print("ok: push differential rejects a missing message, an extra message, a "
          "changed payload field, a lost playing item and a When−EmittedAt offset "
          "that drifted; a sequence mismatch is RED (not 'untested'); an empty "
          "capture is untested, not verified; settle-window leftovers are reported, "
          "and a non-lifecycle one WITHDRAWS the run's greens; a background "
          "broadcast is counted, not blamed on a verb, and an unnamed message "
          "still fails its row; stamps "
          f"{verification.PUSH_DIFF!r} "
          f"(headline stays {verification.HEADLINE!r}); a playing group's "
          "PositionTicks is compared with a printed tolerance, never denylisted")


if __name__ == "__main__":
    main()
