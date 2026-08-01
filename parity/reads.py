#!/usr/bin/env python3
"""Layer-2 read depth with id-correlation (Phase 1 task 6).

Independent scans give the same media file different GUIDs on each server, so a
naive item-scoped read diff compares different titles (the fix-loop's unresolved
item_detail/item_similar noise). This engine aligns items across servers by Path
(identical — both mount the same media), then for each item-scoped endpoint issues
the request with EACH server's own id and deep-diffs the responses with the
volatile denylist. Extends the read set well beyond the seeded 30.

Emits parity/reads-results.json (deep_verified per read op); gen-ledger.py ingests
it, superseding the static seed for the ops it re-verifies live.

Run via sweep.sh (idempotently connects to the already-up servers), or directly:
  HERMIT_URL=... JELLYFIN_URL=... parity/reads.py
Offline self-check:
  parity/reads.py --check
"""
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from sweep import http, get_json, bring_up          # noqa: E402
from parity_diff import diff_counts                  # noqa: E402

CORRELATE_LIMIT = 5   # item-scoped endpoints are exercised against this many Path-aligned items


def token_get(base, path, token):
    st, raw = http("GET", base + path, token)
    if st != 200 or not raw:
        return st, None
    try:
        return st, json.loads(raw)
    except ValueError:
        return st, None

# ---------------------------------------------------------------- endpoint set

def plain(op, url):
    return {"op": op, "kind": "plain", "url": lambda c: url}


def user(op, url):
    # url may contain {u}; filled with the server's user id. Path requested for stable array keys.
    return {"op": op, "kind": "user", "url": lambda c: url.format(u=c["user"])}


def item(op, tmpl):
    # tmpl contains {u} and {i}; filled per server (own user + own correlated item id).
    return {"op": op, "kind": "item", "url": lambda c, i: tmpl.format(u=c["user"], i=i)}


READS = [
    plain("GET /System/Info", "/System/Info"),
    plain("GET /System/Endpoint", "/System/Endpoint"),
    plain("GET /Localization/Cultures", "/Localization/Cultures"),
    plain("GET /Users/Me", "/Users/Me"),
    plain("GET /Sessions", "/Sessions"),
    user("GET /UserViews", "/UserViews?userId={u}"),
    user("GET /Library/MediaFolders", "/Library/MediaFolders"),
    user("GET /Library/VirtualFolders", "/Library/VirtualFolders"),
    user("GET /Items", "/Items?userId={u}&recursive=true&includeItemTypes=Movie&limit=50&sortBy=SortName&fields=Path"),
    user("GET /Items/Latest", "/Items/Latest?userId={u}&limit=20&fields=Path"),
    user("GET /UserItems/Resume", "/UserItems/Resume?userId={u}&limit=12&fields=Path"),
    user("GET /Shows/NextUp", "/Shows/NextUp?userId={u}&limit=24"),
    user("GET /Shows/Upcoming", "/Shows/Upcoming?userId={u}&limit=24"),
    user("GET /Genres", "/Genres?userId={u}"),
    user("GET /Persons", "/Persons?userId={u}&limit=100"),
    user("GET /Studios", "/Studios?userId={u}"),
    user("GET /Items/Filters", "/Items/Filters?userId={u}&includeItemTypes=Movie"),
    user("GET /Items/Filters2", "/Items/Filters2?userId={u}&includeItemTypes=Movie"),
    user("GET /Items/Suggestions", "/Items/Suggestions?userId={u}&limit=10"),
    user("GET /Movies/Recommendations", "/Movies/Recommendations?userId={u}"),
    user("GET /Search/Hints", "/Search/Hints?userId={u}&searchTerm=a&limit=20"),
    # item-scoped — correlated by Path, each server queried with its own id
    item("GET /Items/{itemId}", "/Items/{i}?userId={u}&fields=Path,MediaSources,MediaStreams,Overview,Genres"),
    item("GET /Items/{itemId}/Similar", "/Items/{i}/Similar?userId={u}&limit=12"),
    item("GET /Items/{itemId}/Ancestors", "/Items/{i}/Ancestors?userId={u}"),
    item("GET /Items/{itemId}/PlaybackInfo", "/Items/{i}/PlaybackInfo?userId={u}"),
    item("GET /Items/{itemId}/Images", "/Items/{i}/Images"),
    item("GET /Movies/{itemId}/Similar", "/Movies/{i}/Similar?userId={u}&limit=12"),
]

# ---------------------------------------------------------------- correlation

def path_id_map(base, token, user_id):
    """Path -> id for movies on one server (Path is the stable cross-server key)."""
    b = get_json(base, f"/Items?userId={user_id}&recursive=true&includeItemTypes=Movie"
                       f"&fields=Path&limit=500&sortBy=SortName", token)
    out = {}
    for it in (b or {}).get("Items", []):
        if it.get("Path"):
            out[it["Path"]] = it["Id"]
    return out


def correlate(hmap, jmap):
    """Shared Paths -> list of (hermit_id, jellyfin_id), capped."""
    shared = sorted(set(hmap) & set(jmap))
    return [(hmap[p], jmap[p]) for p in shared[:CORRELATE_LIMIT]]

# ---------------------------------------------------------------- run

def run(hermit_url, jellyfin_url):
    ht, hu = bring_up(hermit_url, "hermit")
    jt, ju = bring_up(jellyfin_url, "jellyfin")
    hc, jc = {"user": hu}, {"user": ju}

    pairs = correlate(path_id_map(hermit_url, ht, hu), path_id_map(jellyfin_url, jt, ju))
    rows = {}

    def record(op, clean, total, buckets):
        if total == 0:
            rows[op] = {"deep_verified": None, "classification": "",
                        "note": "no comparable response (both empty/non-200)"}
            return
        n = sum(len(buckets[k]) for k in ("mismatch", "missing", "extra"))
        if n == 0:
            rows[op] = {"deep_verified": True, "classification": "ok",
                        "note": f"{clean}/{total} clean"}
        else:
            sample = "; ".join(f"{m['path']}(J={m.get('j')} H={m.get('h')})"
                               for m in buckets["mismatch"][:3])
            # Dedup diff paths across the correlated items (strip the per-item [key] prefix) so the
            # detail lists each divergent FIELD once — the actionable enumeration for a fix.
            def field_paths(bucket):
                seen = {}
                for m in bucket:
                    p = re.sub(r"^\[[^\]]*\]\.?", "", m["path"])
                    seen.setdefault(p, m)
                return seen
            rows[op] = {"deep_verified": False,
                        "classification": "flagged: read diff vs Jellyfin (verify)",
                        "note": f"{clean}/{total} clean; mismatch:{len(buckets['mismatch'])} "
                                f"missing:{len(buckets['missing'])} extra:{len(buckets['extra'])} | {sample}",
                        "diffs": {
                            "missing": sorted(field_paths(buckets["missing"])),
                            "extra": sorted(field_paths(buckets["extra"])),
                            "mismatch": sorted(field_paths(buckets["mismatch"])),
                        }}

    for ep in READS:
        if ep["kind"] in ("plain", "user"):
            path = ep["url"](hc if ep["kind"] == "user" else {})
            jpath = ep["url"](jc if ep["kind"] == "user" else {})
            hs, hb = token_get(hermit_url, path, ht)
            js, jb = token_get(jellyfin_url, jpath, jt)
            if hb is None or jb is None:
                record(ep["op"], 0, 0, {})
                continue
            n, buckets = diff_counts(jb, hb)
            record(ep["op"], 1 if n == 0 else 0, 1, buckets)
        else:  # item — aggregate over correlated pairs
            agg = {"mismatch": [], "missing": [], "extra": []}
            clean = tested = 0
            for hid, jid in pairs:
                hs, hb = token_get(hermit_url, ep["url"](hc, hid), ht)
                js, jb = token_get(jellyfin_url, ep["url"](jc, jid), jt)
                if hb is None or jb is None:
                    continue
                tested += 1
                n, b = diff_counts(jb, hb)
                if n == 0:
                    clean += 1
                else:
                    for k in agg:
                        agg[k].extend(b[k])
            record(ep["op"], clean, tested, agg)
    return rows, len(pairs)


def main():
    if "--check" in sys.argv:
        selfcheck()
        return
    hermit = os.environ.get("HERMIT_URL", "http://localhost:18096")
    jellyfin = os.environ.get("JELLYFIN_URL", "http://localhost:18097")
    rows, npairs = run(hermit, jellyfin)
    out = {"generated_by": "parity/reads.py", "last_verified": os.environ.get("PARITY_STAMP", ""),
           "correlated_items": npairs, "rows": rows}
    with open(os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                          "parity/reads-results.json"), "w") as f:
        json.dump(out, f, indent=2, sort_keys=True)
        f.write("\n")
    ok = sum(1 for v in rows.values() if v["deep_verified"] is True)
    print(f"wrote parity/reads-results.json — {len(rows)} read ops, {ok} deep-verified "
          f"(correlated {npairs} items by Path)")


def selfcheck():
    from parity_diff import diff_counts as dc
    # clean vs dirty diff
    assert dc({"A": 1, "Id": "x"}, {"A": 1, "Id": "y"})[0] == 0    # Id volatile → clean
    assert dc({"A": 1}, {"A": 2})[0] == 1                          # real mismatch
    # array align by Path across divergent ids
    j = {"Items": [{"Path": "/m/a.mkv", "Id": "j1", "Name": "A"}]}
    h = {"Items": [{"Path": "/m/a.mkv", "Id": "h1", "Name": "A"}]}
    assert dc(j, h)[0] == 0, "Path-aligned items with divergent Ids should be clean"
    # correlation intersects by Path and caps
    hm = {"/m/a": "h1", "/m/b": "h2", "/m/c": "h3"}
    jm = {"/m/b": "j2", "/m/c": "j3", "/m/d": "j4"}
    global CORRELATE_LIMIT
    pairs = correlate(hm, jm)
    assert pairs == [("h2", "j2"), ("h3", "j3")], pairs
    # every op key is a canonical METHOD /path
    import glob
    spec = json.load(open(sorted(glob.glob(os.path.join(
        os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
        "contracts/jellyfin-openapi-*.json")))[-1]))
    valid = {f"GET {p}" for p in spec["paths"]}
    bad = [ep["op"] for ep in READS if ep["op"] not in valid]
    assert not bad, f"read op-keys not in spec: {bad}"
    print(f"ok: diff, Path-align, correlation, {len(READS)} read op-keys valid")


if __name__ == "__main__":
    main()
