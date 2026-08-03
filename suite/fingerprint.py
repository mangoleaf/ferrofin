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


def capture(base, out):
    reg = json.loads(REGISTRY.read_text())["operations"]
    auth = json.loads(req(f"{base}/Users/AuthenticateByName",
                          body=json.dumps({"Username": "bench", "Pw": "benchpass123"})))
    token, uid = auth["AccessToken"], auth["User"]["Id"]
    # Pick any movie for the {itemId} endpoints.
    items = json.loads(req(f"{base}/Items?userId={uid}&recursive=true&includeItemTypes=Movie&limit=1", token))
    item_id = (items.get("Items") or [{}])[0].get("Id", "")

    fp = {}
    for entry in reg:
        op = entry["op"]
        path = op.split(" ", 1)[1].replace("{itemId}", item_id)
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
    elif len(sys.argv) == 4 and sys.argv[1] == "capture":
        capture(sys.argv[2], sys.argv[3])
    else:
        sys.exit("usage: fingerprint.py capture <base-url> <out.json>  |  --selftest")
