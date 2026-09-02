#!/usr/bin/env python3
"""Terminal phase: the lifecycle ops that END the differential — runs LAST.

  POST /Backup/Restore   204; the server restarts and the PINNED backup's state is live
  POST /System/Restart   204; the server goes away and comes back, in-process (the
                         container keeps running, as Jellyfin's Program.Main loop does)
  POST /System/Shutdown  204; the server goes away and stays away; the container exits
  POST /Backup/Create    LAST of everything, after the pair is docker-restarted and
                         bounded by CREATE_TIMEOUT_S: on a real-size library Jellyfin
                         serializes every entity row-by-row under the pessimistic
                         exclusive DB lock — hours, every concurrent request 500ing
                         "database is locked" (measured 2026-09-01, plan §D0). A
                         timeout is recorded as the measured outcome, never retried,
                         and nothing runs after it on the same pair.

None of these has a body to diff — their observable effect is the liveness timeline
(reachable → unreachable → reachable, or → stays unreachable), a read-back after the
restart, and the container state (`docker compose ps`). Each op runs on BOTH servers in
this order and is `deep_verified` when the effect holds on both, exactly like the write
journeys. The pair is started again afterwards so whatever runs next finds it up.

Emits parity/terminal-results.json; gen-ledger.py ingests it like the journeys.

Run via sweep.sh (after every other layer), or directly against the compose pair:
  FERROFIN_URL=... JELLYFIN_URL=... parity/terminal.py
(Against servers NOT run from suite/perf/docker-compose.yml there is no container to
inspect or start again: the shutdown step leaves both servers down.)
Offline self-check:
  parity/terminal.py --check
"""
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from sweep import (http, get_json, authenticate, bring_up, compose, compose_service, ROOT,  # noqa: E402
                   UP_TIMEOUT_S, api_alive as alive, wait_until as wait_for)
import verification  # noqa: E402

DOWN_TIMEOUT_S = 60      # a drain must start within this
STAY_DOWN_S = 5          # a shutdown must stay down for this long
AUTH_RETRY_S = 0.5       # between login attempts while the API finishes coming up
# UP_TIMEOUT_S (PARITY_UP_TIMEOUT_S), the liveness probe and the poll cadence are sweep.py's —
# one copy, shared with the plugin-install restart in provisioning.


def wait_auth(base):
    """A token once the server is fully up, or None when it never is. Jellyfin answers
    /System/Info/Public before the rest of the API is ready (a plain-text 503 "Server is
    loading" meanwhile), so a login right after the bounce is retried until it succeeds.
    A server that never comes back is a RESULT (the step is flagged), not a crash."""
    deadline = time.monotonic() + UP_TIMEOUT_S
    while True:
        try:
            return authenticate(base)[0]
        except (SystemExit, OSError, ValueError):
            if time.monotonic() >= deadline:
                return None
            time.sleep(AUTH_RETRY_S)


def container_running(base):
    """True/False from `docker compose ps`; None when docker is unavailable."""
    svc = compose_service(base)
    if not svc:
        return None
    rc, out = compose("ps", "-q", "--status", "running", svc)
    return bool(out.strip()) if rc == 0 else None


def bounce_observed(base):
    """The server goes unreachable, then reachable again."""
    return wait_for(base, False, DOWN_TIMEOUT_S) and wait_for(base, True, UP_TIMEOUT_S)


def branding(base, token, text):
    st, _ = http("POST", f"{base}/System/Configuration/Branding", token,
                 json.dumps({"LoginDisclaimer": text, "CustomCss": "", "SplashscreenEnabled": False}))
    return st < 300


def disclaimer(base, token):
    return (get_json(base, "/Branding/Configuration", token) or {}).get("LoginDisclaimer")


def t_restore(base, token):
    """Overwrite a config value → Restore from the PINNED backup (the
    Jellyfin-authored zip seeded with the config volume) → the server restarts
    and the overwrite is gone. Backup/Create is never called here — on a
    real-size library it holds Jellyfin's exclusive DB lock for hours (plan
    §D0); the create op runs LAST, in t_backup_create. The verdict anchors on
    the overwrite being rolled back; `pre` rides in the note so drift between
    the pre-state and the pin's own value is visible to a human."""
    listed = get_json(base, "/Backup", token) or []
    path = next((m.get("Path", "") for m in listed if m.get("Path")), "")
    if not path:
        return False, "no pinned backup listed (was the volume seeded with the backup pin?)"
    before = disclaimer(base, token)
    if not (branding(base, token, "parity-after") and disclaimer(base, token) == "parity-after"):
        branding(base, token, before)
        return False, "setup failed"
    st, _ = http("POST", f"{base}/Backup/Restore", token,
                 json.dumps({"ArchiveFileName": os.path.basename(path)}))
    if st != 204:
        branding(base, token, before)
        return False, f"restore status {st}"
    if not bounce_observed(base):
        branding(base, token, before)
        return False, "no restart observed after restore"
    token = wait_auth(base)
    if token is None:
        return False, "server did not come back after the restore"
    cfg = get_json(base, "/Branding/Configuration", token)
    if cfg is None:
        # A failed read-back must not score as a successful rollback — the pin's
        # own disclaimer is legitimately null, so only the dict distinguishes
        # "read failed" from "value is null".
        return False, "post-restore branding read failed"
    value = cfg.get("LoginDisclaimer")
    # No cleanup write: the restore just reinstated the pin's own config wholesale.
    return value != "parity-after", f"pre={before!r} post-restore={value!r}"


def t_restart(base, token):
    st, _ = http("POST", f"{base}/System/Restart", token, "")
    if st != 204:
        return False, f"status {st}"
    if not bounce_observed(base):
        return False, "no restart observed"
    running = container_running(base)
    return running is not False, f"container running after restart: {running}"


def t_shutdown(base, token):
    st, _ = http("POST", f"{base}/System/Shutdown", token, "")
    if st != 204:
        return False, f"status {st}"
    if not wait_for(base, False, DOWN_TIMEOUT_S):
        return False, "still reachable"
    if wait_for(base, True, STAY_DOWN_S):
        return False, "came back after shutdown"
    running = container_running(base)
    return running is not True, f"container running after shutdown: {running}"


CREATE_TIMEOUT_S = int(os.environ.get("PARITY_BACKUP_CREATE_TIMEOUT_S", "120"))


def t_backup_create(base, token):
    """POST /Backup/Create with the journey's canonical Database-only options,
    bounded by CREATE_TIMEOUT_S. Success = the create response carries the
    manifest (path, echoed options, engine version, date). A timeout is the
    measured outcome on a server that cannot complete the op on this corpus in
    bounded time — recorded, not retried (see the module docstring)."""
    opts = {"Metadata": False, "Trickplay": False, "Subtitles": False, "Database": True}
    t0 = time.monotonic()
    st, raw = http("POST", f"{base}/Backup/Create", token, json.dumps(opts),
                   timeout=CREATE_TIMEOUT_S)
    took = time.monotonic() - t0
    if st == 0 and took >= CREATE_TIMEOUT_S - 1:
        return False, (f"did not complete within {CREATE_TIMEOUT_S}s — holds the exclusive "
                       "DB lock for the whole entity serialization (plan §D0)")
    try:
        created = json.loads(raw)
    except ValueError:
        created = {}
    ok = (st == 200 and bool(created.get("Path")) and created.get("Options") == opts
          and bool(created.get("BackupEngineVersion")) and bool(created.get("DateCreated")))
    return ok, f"status {st} in {took:.1f}s"


STEPS = [
    ("POST /Backup/Restore", t_restore),
    ("POST /System/Restart", t_restart),
    ("POST /System/Shutdown", t_shutdown),
]


def run_one(base, target):
    token, _ = bring_up(base, target)
    out = {}
    for op, step in STEPS:
        if token is None:
            out[op] = (False, "server did not come back after the previous step")
            continue
        try:
            ok, note = step(base, token)
        except Exception as e:   # one step blowing up marks it failed, doesn't abort the rest
            ok, note = False, f"error: {e}"
        out[op] = (ok, note)
        if op != "POST /System/Shutdown":
            token = wait_auth(base)   # the previous step restarted the server
    return out


def classify(h_ok, j_ok):
    if h_ok and j_ok:
        return "ok"
    if h_ok:
        return "flagged: Jellyfin effect differed (verify: oracle setup or Ferrofin extra)"
    if j_ok:
        return "flagged: Ferrofin effect not observed (verify: real gap vs probe method)"
    return "flagged: effect not observed on either server (likely harness/docker access)"


def effect_row(h_pair, j_pair):
    h_ok, h_note = h_pair
    j_ok, j_note = j_pair
    # These ops have no comparable body: the verdict is two independent effect
    # observations AND-ed. So the row says `effect`, never the ledger's
    # body-diff headline.
    return {"deep_verified": bool(h_ok and j_ok), "classification": classify(h_ok, j_ok),
            "verification_method": verification.EFFECT,
            "note": f"H={h_ok} ({h_note}) J={j_ok} ({j_note}) "
                    f"(effect verdict; no body exists to diff)"}


def combine(h, j):
    return {op: effect_row(h.get(op, (False, "not run")), j.get(op, (False, "not run")))
            for op, _ in STEPS}


def restart_pair(*bases):
    """Start whatever the shutdown step stopped so the next stage finds both servers up."""
    for base in bases:
        svc = compose_service(base)
        if not svc:
            print(f"!! {base}: not a compose service — left down", file=sys.stderr)
        elif compose("start", svc)[0] != 0:
            print(f"!! {base}: `docker compose start {svc}` failed — left down", file=sys.stderr)


def main():
    if "--check" in sys.argv:
        selfcheck()
        return
    ferrofin = os.environ.get("FERROFIN_URL", "http://localhost:18096")
    jellyfin = os.environ.get("JELLYFIN_URL", "http://localhost:18097")
    h, j, error = {}, {}, None
    try:
        h = run_one(ferrofin, "ferrofin")
        j = run_one(jellyfin, "jellyfin")
    except Exception as e:   # the verdicts so far are still written, flagged, not lost
        error = f"{type(e).__name__}: {e}"
    finally:
        # Whatever happened, the pair is started again.
        restart_pair(ferrofin, jellyfin)
    rows = combine(h, j)
    # LAST of everything: Backup/Create can wedge a server for the rest of its
    # life (plan §D0), so nothing may run on the pair after it. Ferrofin first —
    # a wedged Jellyfin cannot poison the Ferrofin observation.
    hc = jc = (False, "server not authable after docker restart")
    try:
        tok = wait_auth(ferrofin)
        if tok:
            hc = t_backup_create(ferrofin, tok)
        tok = wait_auth(jellyfin)
        if tok:
            jc = t_backup_create(jellyfin, tok)
    except Exception as e:
        error = error or f"backup-create: {type(e).__name__}: {e}"
    rows["POST /Backup/Create"] = effect_row(hc, jc)
    out = {"generated_by": "suite/parity/terminal.py", "last_verified": os.environ.get("PARITY_STAMP", ""),
           "errors": [error] if error else [], "rows": rows}
    with open(os.path.join(ROOT, "suite/parity/terminal-results.json"), "w") as f:
        json.dump(out, f, indent=2, sort_keys=True)
        f.write("\n")
    ok = sum(1 for v in rows.values() if v["deep_verified"])
    print(f"wrote parity/terminal-results.json — {len(rows)} lifecycle ops, "
          f"{ok} effect-verified (no body exists to diff)")


def selfcheck():
    import glob
    spec = json.load(open(sorted(glob.glob(os.path.join(ROOT, "contracts/jellyfin-openapi-*.json")))[-1]))
    for op in [op for op, _ in STEPS] + ["POST /Backup/Create"]:
        method, path = op.split(" ", 1)
        assert method.lower() in spec["paths"].get(path, {}), op
    # The create row rides the same effect-row shape as the STEPS rows.
    row = effect_row((True, "status 200 in 1.2s"), (False, "did not complete within 120s"))
    assert row["deep_verified"] is False and row["verification_method"] == verification.EFFECT
    assert row["classification"].startswith("flagged: Jellyfin")
    rows = combine({"POST /System/Restart": (True, "x")}, {"POST /System/Restart": (False, "y")})
    assert rows["POST /System/Restart"]["deep_verified"] is False
    assert rows["POST /Backup/Restore"]["note"].startswith("H=False (not run)")
    rows = combine({op: (True, "") for op, _ in STEPS}, {op: (True, "") for op, _ in STEPS})
    assert all(r["deep_verified"] for r in rows.values())
    # Every lifecycle row is an EFFECT verdict. 204 No Content has no body, so a
    # `body-diff` stamp here would be false on its face.
    assert all(r["verification_method"] == verification.EFFECT for r in rows.values())
    print(f"ok: {len(STEPS) + 1} lifecycle op-keys valid, combine logic, all rows stamped "
          f"{verification.EFFECT!r}")


if __name__ == "__main__":
    main()
