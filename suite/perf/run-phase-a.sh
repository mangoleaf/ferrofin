#!/usr/bin/env bash
# Phase A — isolated, open-model, per-endpoint profiling.
#
# For each server: bring it up + scan ONCE, then drive each endpoint on its own
# at a fixed arrival rate (phase_a.py → vegeta, constant arrival rate),
# snapshotting the container's cgroup cpu.stat around each run to attribute
# CPU-seconds/request.
# Produces per-endpoint p50/p95/p99 + CPU-µs/req that are actually comparable
# between Ferrofin and Jellyfin (no cross-endpoint interference, honest tails).
#
#   ./run-phase-a.sh                    both servers
#   BENCH_ONLY=ferrofin ./run-phase-a.sh  one server
#   BENCH_SKIP_BUILD=1 ...              reuse the existing ferrofin-bench:local image
set -euo pipefail
cd "$(dirname "$0")"
# shellcheck source=../lib.sh
source ../lib.sh
suite_load_env
. ./_phase-common.sh
mkdir -p results/raw

# ── knobs (magic numbers surfaced as env, sensible defaults) ────────────────
PHASE_RATE=${PHASE_RATE:-50}         # arrivals/sec per endpoint (open model)
PHASE_DUR=${PHASE_DUR:-20s}          # measured window (≈rate×dur samples ⇒ ~1000 for p99)
PHASE_WARMUP=${PHASE_WARMUP:-5s}     # discarded warm-up (JIT fairness for .NET)
IDLE_SECS=${IDLE_SECS:-5}            # idle-baseline window to subtract background CPU
export PHASE_RATE PHASE_DUR PHASE_WARMUP

# Shared bring-up: library list + passthrough env (suite/lib.sh).
suite_build_libraries

# Endpoint names come from the single source of truth (endpoints.py ENDPOINTS).
# Scenario rows (auth_login) run in their own window elsewhere, never in the
# per-endpoint phase loop.
endpoint_names() {
  python3 -c "from endpoints import ENDPOINTS; [print(e['name']) for e in ENDPOINTS if not e['scenario']]"
}
# usage_usec (total CPU µs charged to the container's cgroup).
# `</dev/null` so `docker exec` can't swallow the endpoint-loop's stdin.
cpu_usec() { docker compose exec -T "$1" cat /sys/fs/cgroup/cpu.stat </dev/null 2>/dev/null | awk '/^usage_usec/{print $2}'; }

profile() {   # $1=service $2=port $3=target
  local svc="$1" base="http://localhost:$2" target="$3"
  bringup_scan "$svc" "$base" "$target" || return 0

  # Idle baseline: CPU the server burns doing nothing, to subtract per-endpoint.
  # `|| echo 0` so a transient `docker exec` hiccup can never abort the run.
  local i0 i1 idle_rate; i0=$(cpu_usec "$svc" || echo 0); sleep "$IDLE_SECS"; i1=$(cpu_usec "$svc" || echo "$i0")
  idle_rate=$(awk -v a="${i0:-0}" -v b="${i1:-0}" -v s="$IDLE_SECS" 'BEGIN{printf "%.0f",(b-a)/s}')
  echo "   idle CPU: ${idle_rate} µs/s"

  local total_s; total_s=$(awk -v w="${PHASE_WARMUP%s}" -v d="${PHASE_DUR%s}" 'BEGIN{print w+d}')
  while IFS= read -r name; do
    local c0 c1 dcpu; c0=$(cpu_usec "$svc" || echo 0)
    local f="results/raw/phaseA-${target}-${name}.json"
    rm -f "$f"   # a failed leg must leave no file — never patch a stale run's numbers
    python3 phase_a.py --target "$target" --base "$base" -e "$name" \
      --rate "$PHASE_RATE" --dur "${PHASE_DUR%s}" --warmup "${PHASE_WARMUP%s}" \
      --out "$f" </dev/null >/dev/null 2>&1 || true
    c1=$(cpu_usec "$svc" || echo "$c0")
    # CPU µs consumed by the endpoint's requests = total delta minus idle burn over the window.
    dcpu=$(awk -v a="${c0:-0}" -v b="${c1:-0}" -v idle="$idle_rate" -v s="$total_s" 'BEGIN{d=(b-a)-idle*s; print (d<0?0:d)}')
    [ -z "$dcpu" ] && dcpu=0
    if [ -f "$f" ]; then
      python3 -c '
import json, sys
p, dcpu = sys.argv[1], float(sys.argv[2])
d = json.load(open(p))
d["cpu_us"] = dcpu
d["cpu_us_per_req"] = dcpu / d["reqs"] if d.get("reqs") else None
open(p, "w").write(json.dumps(d))' "$f" "$dcpu" || true
      printf '   %-24s p50=%s cpu/req=%.1fµs\n' "$name" \
        "$(python3 -c 'import json,sys; v=json.load(open(sys.argv[1])).get("p50"); print("?" if v is None else v)' "$f" 2>/dev/null || echo '?')" \
        "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("cpu_us_per_req") or 0)' "$f" 2>/dev/null || echo 0)" || true
    fi
  done < <(endpoint_names)
  docker compose stop "$svc" >/dev/null 2>&1 || true
}

[ "${BENCH_ONLY:-}" != "jellyfin" ] && profile ferrofin "$FERROFIN_HOST_PORT" ferrofin
[ "${BENCH_ONLY:-}" != "ferrofin" ]   && profile jellyfin "$JELLYFIN_HOST_PORT" jellyfin
docker compose down -v >/dev/null 2>&1 || true

echo ">> rendering Phase A report"
VERSION=$(git -C .. describe --tags --always 2>/dev/null || echo dev)
python3 render_phases.py a "$VERSION" "$PHASE_RATE" "$PHASE_DUR"
echo ">> wrote results/phaseA-${VERSION}.md"
