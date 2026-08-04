#!/usr/bin/env python3
"""suite/fingerprint.py — record a per-operation body "shape" hash for one running server.

The mid-run honesty check (Plan 6): the parity stage captures Hermit's body shapes; the perf
stage captures them again on the fresh perf bring-up. merge.py flags any op whose shape drifted
between the two as `comparable: false` — catching "fast because the body went hollow/wrong since
the last parity pass" at near-zero cost.

Shape, not bytes: parity and perf run on separate bring-ups with fresh DBs, so UUIDs/dates/paths
differ every time. We hash the SET OF DOTTED KEY-PATHS (field presence, array-index-insensitive),
which is stable across DBs but changes the moment Hermit starts omitting fields (genres, people…).

Usage:  python3 suite/fingerprint.py capture <base-url> <out.json>
  ponytail: a spot-check, one probe per variant — not a literal 1-in-N mid-stream sample.
  Upgrade path: sample inside the k6 phase if drift is ever seen slipping through between probes.
"""
import json
import re
import sys
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
REGISTRY = ROOT / "suite" / "registry.json"
CLIENT = 'Client="suite", Device="suite", DeviceId="suite-fp", Version="1.0"'


def req(url, token=None, body=None):
    headers = {"Content-Type": "application/json",
               "Authorization": f"MediaBrowser {'Token=\"'+token+'\", ' if token else ''}{CLIENT}"}
    data = body.encode() if body else None
    with urllib.request.urlopen(urllib.request.Request(url, data=data, headers=headers), timeout=30) as r:
        return r.read()


def key_paths(obj, prefix=""):
    """Yield dotted key-paths, array-index-insensitive: {a:[{b:1}]} → 'a', 'a[].b'."""
    if isinstance(obj, dict):
        for k, v in obj.items():
            p = f"{prefix}.{k}" if prefix else k
            yield p
            yield from key_paths(v, p)
    elif isinstance(obj, list):
        for v in obj:
            yield from key_paths(v, prefix + "[]")


def shape_hash(raw):
    try:
        paths = sorted(set(key_paths(json.loads(raw))))
    except (ValueError, TypeError):
        return "non-json"
    import hashlib
    return hashlib.sha256("\n".join(paths).encode()).hexdigest()[:16]


def capture(base, out, token=None, uid=None):
    reg = json.loads(REGISTRY.read_text())["operations"]
    # A pre-minted token lets the caller skip login here: the perf leg's auth_login
    # scenario throttles Jellyfin's /Users/AuthenticateByName (drops to ~7% success),
    # so a fresh login at post-leg capture time 500s. Reuse a token minted pre-load.
    if not token:
        auth = json.loads(req(f"{base}/Users/AuthenticateByName",
                              body=json.dumps({"Username": "bench", "Pw": "benchpass123"})))
        token, uid = auth["AccessToken"], auth["User"]["Id"]
    # Resolve the ids the expanded endpoint set templates on (mirror of
    # bench-lib's enrichContext; missing shapes resolve to '' and those ops
    # skip below rather than fingerprinting a 404).
    items = json.loads(req(f"{base}/Items?userId={uid}&recursive=true&includeItemTypes=Movie&limit=1", token))
    item_id = (items.get("Items") or [{}])[0].get("Id", "")
    series = json.loads(req(f"{base}/Items?userId={uid}&recursive=true&includeItemTypes=Series&limit=1", token))
    series_id = (series.get("Items") or [{}])[0].get("Id", "")
    tasks = json.loads(req(f"{base}/ScheduledTasks", token))
    task_id = (tasks or [{}])[0].get("Id", "") if isinstance(tasks, list) else ""
    fills = {"{itemId}": item_id, "{seriesId}": series_id, "{taskId}": task_id,
             "{userId}": uid, "{key}": "encoding"}

    fp = {}
    for entry in reg:
        op = entry["op"]
        if not op.startswith("GET "):
            # Write ops are fingerprint-exempt BY DESIGN (a probe would mutate state,
            # and their bodies mint per-run tokens/timestamps); merge.py gates them on
            # the parity write journey (deep_verified) + expected-status instead.
            continue
        path = op.split(" ", 1)[1]
        for var, val in fills.items():
            if val:
                path = path.replace(var, val)
        if "{" in path or "Images" in path:      # unfillable param or binary asset — skip
            continue
        for v in entry["variants"]:
            params = v["params"].replace("{userId}", uid).replace("{itemId}", item_id)
            url = f"{base}{path}" + (f"?{params}" if params else "")
            try:
                fp[op] = shape_hash(req(url, token))
            except Exception as e:
                fp[op] = f"error:{type(e).__name__}"
            break                                 # one variant per op is enough for the shape
    Path(out).write_text(json.dumps(fp, indent=2) + "\n")
    print(f">> {out}: {len(fp)} op fingerprints from {base}")


def _selftest():
    # Same fields, different UUID/date/order → same shape. A dropped field → different shape.
    a = '{"Id":"aaa","Name":"X","Genres":["a"],"People":[{"Name":"p"}]}'
    b = '{"People":[{"Name":"q"}],"Name":"Y","Id":"zzz-1","Genres":["b","c"]}'
    hollow = '{"Id":"aaa","Name":"X","People":[{"Name":"p"}]}'   # Genres dropped
    assert shape_hash(a) == shape_hash(b), "shape must ignore values/order/array length"
    assert shape_hash(a) != shape_hash(hollow), "dropping a field must change the shape"
    assert shape_hash("<html>") == "non-json"
    print("fingerprint self-test OK")


if __name__ == "__main__":
    if len(sys.argv) == 2 and sys.argv[1] == "--selftest":
        _selftest()
    elif len(sys.argv) in (4, 6) and sys.argv[1] == "capture":
        # capture <base-url> <out.json> [<token> <userId>]
        capture(*sys.argv[2:6])
    else:
        sys.exit("usage: fingerprint.py capture <base-url> <out.json> [<token> <userId>]  |  --selftest")
