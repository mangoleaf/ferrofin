#!/usr/bin/env bash
# Phase A — isolated, open-model, per-endpoint profiling.
#
# For each server: bring it up + scan ONCE, then drive each endpoint on its own
# at a fixed arrival rate (phase-a.js, constant-arrival-rate), snapshotting the
# container's cgroup cpu.stat around each run to attribute CPU-seconds/request.
# Produces per-endpoint p50/p95/p99 + CPU-µs/req that are actually comparable
# between Hermit and Jellyfin (no cross-endpoint interference, honest tails).
#
#   ./run-phase-a.sh                    both servers
#   BENCH_ONLY=hermit ./run-phase-a.sh  one server
#   BENCH_SKIP_BUILD=1 ...              reuse the existing hermit-bench:local image
set -euo pipefail
cd "$(dirname "$0")"
set -a; [ -f .env ] || cp .env.example .env; . ./.env; set +a
. ./_phase-common.sh
mkdir -p results/raw

# ── knobs (magic numbers surfaced as env, sensible defaults) ────────────────
PHASE_RATE=${PHASE_RATE:-50}         # arrivals/sec per endpoint (open model)
PHASE_DUR=${PHASE_DUR:-20s}          # measured window (≈rate×dur samples ⇒ ~1000 for p99)
PHASE_WARMUP=${PHASE_WARMUP:-5s}     # discarded warm-up (JIT fairness for .NET)
IDLE_SECS=${IDLE_SECS:-5}            # idle-baseline window to subtract background CPU
export PHASE_RATE PHASE_DUR PHASE_WARMUP

# Library list — identical construction to run.sh (parsed by k6, no bash splitting).
LIBS="["; sep=""
[ -n "${REAL_MEDIA_DIR:-}" ] && { LIBS="$LIBS${sep}{\"name\":\"Movies\",\"type\":\"movies\",\"path\":\"/media/movies-real\"}"; sep=","; }
[ -n "${REAL_TV_DIR:-}" ]    && { LIBS="$LIBS${sep}{\"name\":\"Shows\",\"type\":\"tvshows\",\"path\":\"/media/tv-real\"}"; sep=","; }
[ "${FIXTURE_MOVIES:-0}" -gt 0 ] && { LIBS="$LIBS${sep}{\"name\":\"Movies (synth)\",\"type\":\"movies\",\"path\":\"/media/synth/movies\"}"; sep=","; }
[ "${FIXTURE_SERIES:-0}" -gt 0 ] && { LIBS="$LIBS${sep}{\"name\":\"Shows (synth)\",\"type\":\"tvshows\",\"path\":\"/media/synth/tv\"}"; sep=","; }
LIBS="$LIBS]"
[ "$LIBS" = "[]" ] && { echo "No media: set REAL_MEDIA_DIR or FIXTURE_MOVIES>0 in .env"; exit 1; }
if [ -n "${REAL_MEDIA_DIR:-}" ] || [ -n "${REAL_TV_DIR:-}" ]; then EXPECTED_ITEMS=0
else EXPECTED_ITEMS=$(( ${FIXTURE_MOVIES:-0} + ${FIXTURE_SERIES:-0} * ${FIXTURE_EPISODES_PER_SERIES:-0} )); fi
export LIBRARIES="$LIBS" REAL_MEDIA_DIR REAL_TV_DIR BENCH_ADMIN_USER BENCH_ADMIN_PASSWORD EXPECTED_ITEMS JELLYFIN_IMAGE

# Endpoint names come from the single source of truth (bench-lib ENDPOINTS),
# read via node with the k6 imports stripped.
endpoint_names() {
  node -e '
    const fs=require("fs");
    let s=fs.readFileSync("bench-lib.js","utf8").replace(/^import .*$/gm,"").replace(/^export /gm,"");
    s+="\nENDPOINTS.forEach(e=>console.log(e.name));";
    new Function("__ENV", s)({});'
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
    k6 run -e ENDPOINT="$name" -e TARGET="$target" -e BASE_URL="$base" phase-a.js </dev/null >/dev/null 2>&1 || true
    c1=$(cpu_usec "$svc" || echo "$c0")
    # CPU µs consumed by the endpoint's requests = total delta minus idle burn over the window.
    dcpu=$(awk -v a="${c0:-0}" -v b="${c1:-0}" -v idle="$idle_rate" -v s="$total_s" 'BEGIN{d=(b-a)-idle*s; print (d<0?0:d)}')
    [ -z "$dcpu" ] && dcpu=0
    local f="results/raw/phaseA-${target}-${name}.json"
    if [ -f "$f" ]; then
      node -e "const d=require('./$f'); d.cpu_us=$dcpu; d.cpu_us_per_req=d.reqs?($dcpu/d.reqs):null; require('fs').writeFileSync('./$f',JSON.stringify(d));" || true
      printf '   %-24s p50=%s cpu/req=%.1fµs\n' "$name" "$(node -pe "require('./$f').p50" 2>/dev/null || echo '?')" "$(node -pe "require('./$f').cpu_us_per_req||0" 2>/dev/null || echo 0)" || true
    fi
  done < <(endpoint_names)
  docker compose stop "$svc" >/dev/null 2>&1 || true
}

[ "${BENCH_ONLY:-}" != "jellyfin" ] && profile hermit "$HERMIT_HOST_PORT" hermit
[ "${BENCH_ONLY:-}" != "hermit" ]   && profile jellyfin "$JELLYFIN_HOST_PORT" jellyfin
docker compose down -v >/dev/null 2>&1 || true

echo ">> rendering Phase A report"
VERSION=$(git -C .. describe --tags --always 2>/dev/null || echo dev)
node render-phase-a.mjs "$VERSION" "$PHASE_RATE" "$PHASE_DUR"
echo ">> wrote results/phaseA-${VERSION}.md"
