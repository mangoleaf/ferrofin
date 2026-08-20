#!/usr/bin/env bash
# Drives ONE endpoint at ONE arrival rate against the fast-loop server and prints
# p50/p95/p99 plus the success rate. ~10s per measurement. See README.md.
#
#   ./hit.sh persons 608
#   ./hit.sh nextup 2000 20        # 20s window instead of the default 10
#
# Endpoint names and their paths come from suite/perf/endpoints.py — the same
# single source of truth the real gate uses, so a name here means the same
# request there — plus the fast-loop-only write rows in ./write_endpoints.py.
# Fixtures (userId, an item id, a playlist id) are resolved once and cached in
# $FF_MICRO_CTX (default /tmp/ff-micro-ctx.json).
#
# Write rows (any non-GET, with or without a JSON body) drive through a vegeta
# TARGETS file rather than the one-line form, because vegeta only reads a body
# from `@file` in a targets block. `./hit.sh list` prints every drivable name.
set -euo pipefail
cd "$(dirname "$0")"

NAME=${1:?usage: hit.sh <endpoint-name|list> <rate> [secs]}
if [ "$NAME" = list ]; then RATE=0; else
  RATE=${2:?usage: hit.sh <endpoint-name|list> <rate> [secs]}
fi
SECS=${3:-10}
PORT=${FF_MICRO_PORT:-18299}
BASE="http://127.0.0.1:$PORT"

command -v vegeta >/dev/null || { echo "vegeta not on PATH: eval \"\$(mise env -s bash)\" in suite/perf" >&2; exit 1; }
curl -sf "$BASE/System/Info/Public" >/dev/null || { echo "no server on $BASE — ./serve.sh start" >&2; exit 1; }

python3 - "$NAME" "$RATE" "$SECS" "$BASE" <<'PY'
import json, os, subprocess, sys, pathlib, tempfile
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent / "perf"))
sys.path.insert(0, "../perf")
import benchlib, endpoints, write_endpoints

name, rate, secs, base = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]

# The gate's table first, then the fast-loop-only write rows. A name collision
# would silently change what a gate name means, so it is an error.
defs = {e["name"]: e for e in endpoints.ENDPOINTS}
for e in write_endpoints.WRITE_ENDPOINTS:
    if e["name"] in defs:
        sys.exit(f"write_endpoints.py redefines gate endpoint {e['name']!r}")
    defs[e["name"]] = e

if name == "list":
    for n, e in sorted(defs.items()):
        print(f"  {e.get('method','GET'):6} {n}")
    sys.exit(0)
if name not in defs:
    near = [n for n in defs if name in n]
    sys.exit(f"unknown endpoint {name!r}" + (f"; did you mean {near}?" if near else ""))
ep = defs[name]
if ep.get("scenario"):
    sys.exit(f"{name} is a scenario endpoint — not supported by the fast loop")

# Fixtures are expensive to resolve (they query the server for real ids), so do
# it once and cache. Delete the cache file to force a refresh.
CACHE = pathlib.Path(os.environ.get("FF_MICRO_CTX", "/tmp/ff-micro-ctx.json"))
if CACHE.exists():
    ctx = json.loads(CACHE.read_text())
else:
    ctx = benchlib.authenticate(base, "ferrofin")
    benchlib.pick_items(base, ctx)
    CACHE.write_text(json.dumps(ctx))

def render(ctx):
    """The concrete path + body for this context (both are format strings)."""
    return (ep["path"].format(**ctx),
            benchlib.render_body(ep["body"], ctx) if ep["body"] is not None else None)

try:
    path, body = render(ctx)
except KeyError:
    # The write rows need the ids only `enrich_context` resolves (writeItemId,
    # playlistId, …). Resolve them on demand and extend the cache, so a GET-only
    # cache written by an older run still works.
    benchlib.enrich_context(base, ctx)
    CACHE.write_text(json.dumps(ctx))
    try:
        path, body = render(ctx)
    except KeyError as e:
        sys.exit(f"{name} needs fixture {e} the server could not provide; "
                 f"delete {CACHE} and retry, or the endpoint is unsupported here")

hdr = (f'Authorization: MediaBrowser Token="{ctx["token"]}", Client="micro", '
       f'Device="micro", DeviceId="micro-fastloop", Version="1.0"')
method = ep.get("method", "GET")

# vegeta reads a request body only from an `@file` line inside a TARGETS block,
# never from the one-line form — so writes go through a targets file. GETs keep
# the original one-line target verbatim.
attack = ["vegeta", "attack", "-rate", f"{rate}/1s", "-duration", f"{secs}s",
          "-header", hdr, "-timeout", "30s", "-max-workers", "4096"]
with tempfile.TemporaryDirectory() as tmp:
    target = f"{method} {base}{path}\n"
    if body is not None:
        bodyfile = pathlib.Path(tmp) / "body.json"
        bodyfile.write_text(json.dumps(body))
        target += f"Content-Type: application/json\n@{bodyfile}\n"
    out = subprocess.run(attack, input=target.encode(),
                         stdout=subprocess.PIPE, check=True).stdout
rep = subprocess.run(["vegeta", "report", "-type", "json"],
                     input=out, stdout=subprocess.PIPE, check=True).stdout
r = json.loads(rep)

ms = lambda ns: ns / 1e6
lat = r["latencies"]
ok = 100.0 * r["success"]
achieved = r["rate"]
# A window that could not hold its schedule is not an open-loop measurement at
# the requested rate — the generator itself became the bottleneck, so the
# percentiles describe the harness, not the server. Flag it rather than report it.
held = achieved >= 0.99 * float(rate)

codes = r.get("status_codes") or {}
expected = str(ep.get("ok", 200))
off = {c: n for c, n in codes.items() if c != expected}

print(f"{name}  @{rate}/s x{secs}s")
print(f"  p50 {ms(lat['50th']):9.1f} ms   p95 {ms(lat['95th']):9.1f} ms   "
      f"p99 {ms(lat['99th']):9.1f} ms   max {ms(lat['max']):9.1f} ms")
print(f"  ok  {ok:5.1f}%   requests {r['requests']}   achieved {achieved:.0f}/s"
      + ("" if held else "   <-- RATE NOT HELD, percentiles unusable"))
# A write row that answers 200 where 204 is the contract (or vice versa) is
# still "success" to vegeta but is NOT the request under test — surface it.
if off:
    print(f"  <-- expected {expected}, also saw {off}")
if r.get("errors"):
    print(f"  errors: {r['errors'][:3]}")
PY
