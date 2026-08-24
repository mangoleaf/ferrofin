#!/usr/bin/env python3
"""Terminal phase: the three lifecycle ops that END the differential — runs LAST.

  POST /Backup/Restore   204; the server restarts and the backed-up state is live again
  POST /System/Restart   204; the server goes away and comes back, in-process (the
                         container keeps running, as Jellyfin's Program.Main loop does)
  POST /System/Shutdown  204; the server goes away and stays away; the container exits

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
    """Backup with a distinctive config value → overwrite it → Restore → the server restarts
    and the backed-up value is live again."""
    ok = branding(base, token, "parity-before")
    st, raw = http("POST", f"{base}/Backup/Create", token, json.dumps({"Database": True}))
    try:
        path = json.loads(raw).get("Path") or ""
    except ValueError:
        path = ""
    ok = ok and st == 200 and bool(path) and branding(base, token, "parity-after")
    ok = ok and disclaimer(base, token) == "parity-after"
    if not ok:
        branding(base, token, "")
        return False, "setup failed"
    st, _ = http("POST", f"{base}/Backup/Restore", token,
                 json.dumps({"ArchiveFileName": os.path.basename(path)}))
    if st != 204:
        branding(base, token, "")
        return False, f"restore status {st}"
    if not bounce_observed(base):
        branding(base, token, "")
        return False, "no restart observed after restore"
    token = wait_auth(base)
    if token is None:
        return False, "server did not come back after the restore"
    value = disclaimer(base, token)
    branding(base, token, "")   # leave the config as the harness found it
    return value == "parity-before", f"post-restore LoginDisclaimer={value!r}"


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


def combine(h, j):
    rows = {}
    for op, _ in STEPS:
        h_ok, h_note = h.get(op, (False, "not run"))
        j_ok, j_note = j.get(op, (False, "not run"))
        if h_ok and j_ok:
            cls = "ok"
        elif h_ok:
            cls = "flagged: Jellyfin effect differed (verify: oracle setup or Ferrofin extra)"
        elif j_ok:
            cls = "flagged: Ferrofin effect not observed (verify: real gap vs probe method)"
        else:
            cls = "flagged: effect not observed on either server (likely harness/docker access)"
        rows[op] = {"deep_verified": bool(h_ok and j_ok), "classification": cls,
                    "note": f"H={h_ok} ({h_note}) J={j_ok} ({j_note})"}
    return rows


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
    out = {"generated_by": "suite/parity/terminal.py", "last_verified": os.environ.get("PARITY_STAMP", ""),
           "errors": [error] if error else [], "rows": rows}
    with open(os.path.join(ROOT, "suite/parity/terminal-results.json"), "w") as f:
        json.dump(out, f, indent=2, sort_keys=True)
        f.write("\n")
    ok = sum(1 for v in rows.values() if v["deep_verified"])
    print(f"wrote parity/terminal-results.json — {len(rows)} lifecycle ops, {ok} deep-verified")


def selfcheck():
    import glob
    spec = json.load(open(sorted(glob.glob(os.path.join(ROOT, "contracts/jellyfin-openapi-*.json")))[-1]))
    for op, _ in STEPS:
        method, path = op.split(" ", 1)
        assert method.lower() in spec["paths"].get(path, {}), op
    rows = combine({"POST /System/Restart": (True, "x")}, {"POST /System/Restart": (False, "y")})
    assert rows["POST /System/Restart"]["deep_verified"] is False
    assert rows["POST /Backup/Restore"]["note"].startswith("H=False (not run)")
    rows = combine({op: (True, "") for op, _ in STEPS}, {op: (True, "") for op, _ in STEPS})
    assert all(r["deep_verified"] for r in rows.values())
    print(f"ok: {len(STEPS)} lifecycle op-keys valid, combine logic")


if __name__ == "__main__":
    main()
