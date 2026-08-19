#!/usr/bin/env bash
# Drives ONE endpoint at ONE arrival rate against the fast-loop server and prints
# p50/p95/p99 plus the success rate. ~10s per measurement. See README.md.
#
#   ./hit.sh persons 608
#   ./hit.sh nextup 2000 20        # 20s window instead of the default 10
#
# Endpoint names and their paths come from suite/perf/endpoints.py — the same
# single source of truth the real gate uses, so a name here means the same
# request there. Fixtures (userId, an item id, a playlist id) are resolved once
# and cached in /tmp/ff-micro-ctx.json.
set -euo pipefail
cd "$(dirname "$0")"

NAME=${1:?usage: hit.sh <endpoint-name> <rate> [secs]}
RATE=${2:?usage: hit.sh <endpoint-name> <rate> [secs]}
SECS=${3:-10}
PORT=${FF_MICRO_PORT:-18299}
BASE="http://127.0.0.1:$PORT"

command -v vegeta >/dev/null || { echo "vegeta not on PATH: eval \"\$(mise env -s bash)\" in suite/perf" >&2; exit 1; }
curl -sf "$BASE/System/Info/Public" >/dev/null || { echo "no server on $BASE — ./serve.sh start" >&2; exit 1; }

python3 - "$NAME" "$RATE" "$SECS" "$BASE" <<'PY'
import json, os, subprocess, sys, pathlib
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent / "perf"))
sys.path.insert(0, "../perf")
import benchlib, endpoints

name, rate, secs, base = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]

defs = {e["name"]: e for e in endpoints.ENDPOINTS}
if name not in defs:
    near = [n for n in defs if name in n]
    sys.exit(f"unknown endpoint {name!r}" + (f"; did you mean {near}?" if near else ""))
ep = defs[name]
if ep.get("scenario"):
    sys.exit(f"{name} is a scenario endpoint — not supported by the fast loop")

# Fixtures are expensive to resolve (they query the server for real ids), so do
# it once and cache. Delete /tmp/ff-micro-ctx.json to force a refresh.
CACHE = pathlib.Path("/tmp/ff-micro-ctx.json")
if CACHE.exists():
    ctx = json.loads(CACHE.read_text())
else:
    ctx = benchlib.authenticate(base, "ferrofin")
    benchlib.pick_items(base, ctx)
    CACHE.write_text(json.dumps(ctx))

path = ep["path"]
try:
    path = path.format(**ctx)
except KeyError as e:
    sys.exit(f"{name} needs fixture {e} which pick_items did not provide; "
             f"delete /tmp/ff-micro-ctx.json and retry, or the endpoint is unsupported here")

target = f"{ep.get('method','GET')} {base}{path}\n"
hdr = (f'Authorization: MediaBrowser Token="{ctx["token"]}", Client="micro", '
       f'Device="micro", DeviceId="micro-fastloop", Version="1.0"')

out = subprocess.run(
    ["vegeta", "attack", "-rate", f"{rate}/1s", "-duration", f"{secs}s",
     "-header", hdr, "-timeout", "30s", "-max-workers", "4096"],
    input=target.encode(), stdout=subprocess.PIPE, check=True).stdout
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

print(f"{name}  @{rate}/s x{secs}s")
print(f"  p50 {ms(lat['50th']):9.1f} ms   p95 {ms(lat['95th']):9.1f} ms   "
      f"p99 {ms(lat['99th']):9.1f} ms   max {ms(lat['max']):9.1f} ms")
print(f"  ok  {ok:5.1f}%   requests {r['requests']}   achieved {achieved:.0f}/s"
      + ("" if held else "   <-- RATE NOT HELD, percentiles unusable"))
if r.get("errors"):
    print(f"  errors: {r['errors'][:3]}")
PY
