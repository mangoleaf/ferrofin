#!/usr/bin/env python3
"""suite/fingerprint.py — record a per-variant body "shape" for one running server.

The mid-run honesty check: the perf leg captures each benched GET variant's body shape on
both servers. merge.py then uses the captures two ways (see merge.py's shape_check):
  - Ferrofin's shape vs the committed shape baseline (suite/results/shape-baseline.json) is
    the EXCLUDER — "the body changed since the last reviewed run" catches "fast because the
    body went hollow" without permanently excluding documented Jellyfin divergences;
  - Ferrofin's shape vs Jellyfin's from the same leg is INFORMATIONAL — published on the row
    so a cross-server divergence is visible, never a silent exclusion (the parity ledger,
    not the bench, owns the Ferrofin-vs-Jellyfin body verdict).

Shape, not bytes: bring-ups use fresh DBs, so UUIDs/dates/paths differ every time. We hash
the SET OF DOTTED KEY-PATHS (field presence, array-index-insensitive), which is stable
across DBs but changes the moment Ferrofin starts omitting fields (genres, people…).
Dict keys that are themselves values — Jellyfin keys ImageBlurHashes by the per-instance
md5 image tag — collapse to "*" so a value never leaks into the key space.

Output: {variant_id: {"hash": <16-hex|"non-json"|"error:...">, "paths": [...]}} — the paths
are kept so a drifting variant in a record is diagnosable without re-running two servers.

Usage:  python3 suite/fingerprint.py capture <base-url> <out.json> [<token> <userId>]
"""
import json
import re
import sys
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
REGISTRY = ROOT / "suite" / "registry.json"
CLIENT = 'Client="suite", Device="suite", DeviceId="suite-fp", Version="1.0"'

# A dict key that is itself a derived value (md5/sha-style hex, e.g. the image-tag keys of
# ImageBlurHashes.<type>): per-instance by construction, so it collapses to "*" — shape
# hashing ignores values by design, and these are values in key position.
HEX_KEY = re.compile(r"^[0-9a-fA-F]{16,}$")


def req(url, token=None, body=None):
    headers = {"Content-Type": "application/json",
               "Authorization": f"MediaBrowser {'Token=\"'+token+'\", ' if token else ''}{CLIENT}"}
    data = body.encode() if body else None
    with urllib.request.urlopen(urllib.request.Request(url, data=data, headers=headers), timeout=30) as r:
        return r.read()


def key_paths(obj, prefix=""):
    """Yield dotted key-paths, array-index-insensitive: {a:[{b:1}]} → 'a', 'a[].b'.
    Hex-hash keys collapse: {"BlurHashes":{"Primary":{"3f9a…":"x"}}} → 'BlurHashes.Primary.*'."""
    if isinstance(obj, dict):
        for k, v in obj.items():
            seg = "*" if HEX_KEY.match(k) else k
            p = f"{prefix}.{seg}" if prefix else seg
            yield p
            yield from key_paths(v, p)
    elif isinstance(obj, list):
        for v in obj:
            yield from key_paths(v, prefix + "[]")


def shape(raw):
    """{"hash", "paths"} for one response body."""
    try:
        paths = sorted(set(key_paths(json.loads(raw))))
    except (ValueError, TypeError):
        return {"hash": "non-json", "paths": []}
    import hashlib
    return {"hash": hashlib.sha256("\n".join(paths).encode()).hexdigest()[:16], "paths": paths}


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
    # benchlib's enrich_context; missing shapes resolve to '' and those ops
    # skip below rather than fingerprinting a 404).
    items = json.loads(req(f"{base}/Items?userId={uid}&recursive=true&includeItemTypes=Movie&limit=1", token))
    item_id = (items.get("Items") or [{}])[0].get("Id", "")
    series = json.loads(req(f"{base}/Items?userId={uid}&recursive=true&includeItemTypes=Series&limit=1", token))
    series_id = (series.get("Items") or [{}])[0].get("Id", "")
    tasks = json.loads(req(f"{base}/ScheduledTasks", token))
    task_id = (tasks or [{}])[0].get("Id", "") if isinstance(tasks, list) else ""
    fills = {"{itemId}": item_id, "{seriesId}": series_id, "{taskId}": task_id,
             "{userId}": uid, "{key}": "encoding"}

    # Ops whose spec path param is spelled {itemId} but whose bench variant
    # targets a DIFFERENT entity. Filling them from the generic movie id would
    # hash a different subject than the row measures — a fingerprint that
    # matches (or drifts) for the wrong reason. '' means "skip this op": a
    # missing fingerprint only turns the shape check off (merge.py), whereas a
    # wrong one is an actively misleading signal.
    per_op = {
        "GET /Shows/{itemId}/Similar": series_id,
        "GET /Playlists/{itemId}/InstantMix": "",   # no playlist fixture here
    }

    fp = {}
    for entry in reg:
        op = entry["op"]
        if not op.startswith("GET "):
            # Write ops are fingerprint-exempt BY DESIGN (a probe would mutate state,
            # and their bodies mint per-run tokens/timestamps); merge.py gates them on
            # the parity write journey (deep_verified) + expected-status instead.
            continue
        path = op.split(" ", 1)[1]
        if op in per_op:
            if not per_op[op]:
                continue
            path = path.replace("{itemId}", per_op[op])
        for var, val in fills.items():
            if val:
                path = path.replace(var, val)
        if "{" in path or "Images" in path:      # unfillable param or binary asset — skip
            continue
        # EVERY variant gets its own shape: one op-level probe used to gate all of an
        # op's rows, so a movie-list capture excluded the Episode/Series/BoxSet rows too.
        for v in entry["variants"]:
            params = v["params"].replace("{userId}", uid).replace("{itemId}", item_id)
            url = f"{base}{path}" + (f"?{params}" if params else "")
            try:
                fp[v["id"]] = shape(req(url, token))
            except Exception as e:
                fp[v["id"]] = {"hash": f"error:{type(e).__name__}", "paths": []}
    Path(out).write_text(json.dumps(fp, indent=2) + "\n")
    print(f">> {out}: {len(fp)} variant fingerprints from {base}")


def _selftest():
    # Same fields, different UUID/date/order → same shape. A dropped field → different shape.
    a = '{"Id":"aaa","Name":"X","Genres":["a"],"People":[{"Name":"p"}]}'
    b = '{"People":[{"Name":"q"}],"Name":"Y","Id":"zzz-1","Genres":["b","c"]}'
    hollow = '{"Id":"aaa","Name":"X","People":[{"Name":"p"}]}'   # Genres dropped
    assert shape(a)["hash"] == shape(b)["hash"], "shape must ignore values/order/array length"
    assert shape(a)["hash"] != shape(hollow)["hash"], "dropping a field must change the shape"
    assert shape("<html>")["hash"] == "non-json"
    # Hex-hash dict keys (ImageBlurHashes' md5 image-tag keys) are values in key position:
    # they collapse to "*" so two instances with different tags still match…
    t1 = '{"ImageBlurHashes":{"Primary":{"0288491bb753f1def201da233a6facbc":"LEHV6"}}}'
    t2 = '{"ImageBlurHashes":{"Primary":{"ec28457aaff9089873e2bdbbe797033b":"WgHz;"}}}'
    assert shape(t1)["hash"] == shape(t2)["hash"], "hex-hash keys must collapse"
    assert "ImageBlurHashes.Primary.*" in shape(t1)["paths"]
    # …while dropping the blurhash dict entirely still changes the shape.
    assert shape(t1)["hash"] != shape('{"ImageBlurHashes":{}}')["hash"]
    # Ordinary PascalCase keys (shorter than 16 hex chars or non-hex) never collapse.
    assert "Genres" in shape(a)["paths"] and "People[].Name" in shape(a)["paths"]
    print("fingerprint self-test OK")


if __name__ == "__main__":
    if len(sys.argv) == 2 and sys.argv[1] == "--selftest":
        _selftest()
    elif len(sys.argv) in (4, 6) and sys.argv[1] == "capture":
        # capture <base-url> <out.json> [<token> <userId>]
        capture(*sys.argv[2:6])
    else:
        sys.exit("usage: fingerprint.py capture <base-url> <out.json> [<token> <userId>]  |  --selftest")
