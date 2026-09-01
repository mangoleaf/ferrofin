"""Persist a truncated SAMPLE of both servers' responses beside every body diff.

The suite used to fetch two bodies, diff them, write one sentence ("3 fields
differ"), and throw both bodies away. That makes every verdict unauditable: you
cannot see what Jellyfin returned that Ferrofin did not, you cannot tell a real
divergence from a probe artifact, and a wrong verdict looks exactly like a right
one. This module keeps the evidence.

Samples are TRUNCATED, not complete: the point is to show the shape and the
representative values of a response, not to archive it. A full capture of the
whole surface would be tens of MB of committed churn every run and nobody would
read it. Limits live in suite/bench.conf (SAMPLE_*) so they are tunable without
editing code.

Written to suite/parity/samples.json as {op: {route, jellyfin, ferrofin, diff}}.
"""
import json
import os
from pathlib import Path

HERE = Path(__file__).resolve().parent
OUT = HERE / "samples.json"


def _conf(name, default):
    """bench.conf < env — the same resolution order as the rest of the suite."""
    if os.environ.get(name):
        return int(os.environ[name])
    try:
        for line in (HERE.parent / "bench.conf").read_text().splitlines():
            line = line.strip()
            if line.startswith(f"{name}="):
                return int(line.split("=", 1)[1].split("#")[0].strip())
    except (OSError, ValueError):
        pass
    return default


MAX_ARRAY = _conf("SAMPLE_MAX_ARRAY", 2)      # elements kept per list
MAX_STRING = _conf("SAMPLE_MAX_STRING", 300)  # chars kept per string
MAX_BYTES = _conf("SAMPLE_MAX_BYTES", 20000)  # cap per op, per side

_captured = {}


def truncate(o, limit=None):
    """Shape-preserving truncation. Lists keep their first `limit` elements and
    say how many were dropped; long strings are cut with an explicit marker. The
    result is still valid JSON, so it renders and diffs like the real thing."""
    lim = MAX_ARRAY if limit is None else limit
    if isinstance(o, dict):
        return {k: truncate(v, lim) for k, v in o.items()}
    if isinstance(o, list):
        head = [truncate(v, lim) for v in o[:lim]]
        if len(o) > lim:
            head.append(f"... +{len(o) - lim} more of {len(o)}")
        return head
    if isinstance(o, str) and len(o) > MAX_STRING:
        return o[:MAX_STRING] + f"... (+{len(o) - MAX_STRING} chars)"
    return o


def _fit(o):
    """If a truncated side still exceeds MAX_BYTES, cut arrays harder rather than
    emit an unreadable wall — never silently; the marker stays in the document."""
    if len(json.dumps(o)) <= MAX_BYTES:
        return o
    o = truncate(o, 1)
    s = json.dumps(o)
    if len(s) <= MAX_BYTES:
        return o
    return {"_truncated": f"exceeded SAMPLE_MAX_BYTES ({MAX_BYTES}) at 1 element/array",
            "_head": s[:MAX_BYTES]}


def record(op, jellyfin, ferrofin, route=None, diff=None):
    """Capture one op's pair. First writer wins, so a layer with a better probe
    should call `replace`."""
    if op in _captured:
        return
    _captured[op] = {"route": route or op,
                     "jellyfin": _fit(truncate(jellyfin)),
                     "ferrofin": _fit(truncate(ferrofin)),
                     "diff": diff}


def replace(op, jellyfin, ferrofin, route=None, diff=None):
    """Overwrite an earlier capture (a curated probe beats the broad sweep)."""
    _captured.pop(op, None)
    record(op, jellyfin, ferrofin, route, diff)


def flush():
    """Merge with whatever an earlier layer wrote, so the last layer to finish
    adds to the file instead of clobbering it."""
    if not _captured:
        return
    try:
        for k, v in json.loads(OUT.read_text()).get("samples", {}).items():
            _captured.setdefault(k, v)
    except (OSError, ValueError):
        pass
    OUT.write_text(json.dumps(
        {"generated_by": "suite/parity/samples.py",
         "limits": {"max_array": MAX_ARRAY, "max_string": MAX_STRING, "max_bytes": MAX_BYTES},
         "samples": dict(sorted(_captured.items()))}, indent=2) + "\n")
    print(f">> wrote suite/parity/samples.json ({len(_captured)} endpoint pairs)")


def _selftest():
    assert truncate([1, 2, 3, 4]) == [1, 2, "... +2 more of 4"]
    assert truncate({"a": ["x" * 400]})["a"][0].endswith("(+100 chars)")
    assert truncate({"n": 1, "b": None}) == {"n": 1, "b": None}
    assert len(json.dumps(_fit(truncate({"Items": [{"Name": f"n{i}"} for i in range(500)]})))) <= MAX_BYTES
    record("GET /X", {"a": 1}, {"a": 2}, route="/X")
    record("GET /X", {"a": 9}, {"a": 9})
    assert _captured["GET /X"]["jellyfin"] == {"a": 1}, "first writer must win"
    replace("GET /X", {"a": 9}, {"a": 9})
    assert _captured["GET /X"]["jellyfin"] == {"a": 9}, "replace must overwrite"
    _captured.clear()
    print("samples selftest ok")


if __name__ == "__main__":
    _selftest()
