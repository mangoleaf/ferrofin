#!/usr/bin/env bash
# Phase B — per-endpoint saturation sweep → max sustainable RPS + the knee (G2).
#
# For each endpoint, drive it (open model, phase_a.py → vegeta) at increasing
# arrival rates until the server can no longer keep up — the point where the
# generator starts dropping arrivals (dropped > 0) or responses stop being 200.
# The last rate it served cleanly is that endpoint's max sustainable
# throughput. Two numbers per endpoint, deliberately separate from the
# fixed-rate latency comparison (max-throughput and latency must never share
# a headline):
#   max_rps    the last cleanly-served rate
#   knee_rate  the LOWEST rate at which p99 exceeded BENCH_KNEE_P99_MS —
#              where latency departs, usually well before hard saturation
# Scoped to a curated set of endpoints (a full sweep would run for hours);
# override with PHASE_B_ENDPOINTS.
#
#   ./run-phase-b.sh                    both servers, curated endpoints
#   BENCH_ONLY=ferrofin ./run-phase-b.sh
set -euo pipefail
cd "$(dirname "$0")"
# shellcheck source=../lib.sh
source ../lib.sh
suite_load_env .env
suite_mint_device_id phase-b
. ./_phase-common.sh
mkdir -p results/raw

SWEEP_RATES=${SWEEP_RATES:-"25 50 100 200 400 800 1600 3200 6400"}   # req/s ladder
SWEEP_DUR=${SWEEP_DUR:-8s}                                           # per-rate window (short)
SWEEP_WARMUP=${SWEEP_WARMUP:-2s}
PHASE_B_ENDPOINTS=${PHASE_B_ENDPOINTS:-"info_public user_me items_sortname items_mixed item_detail persons studios suggestions movie_recommendations items_filters2 image_primary"}

# Library list (shared bring-up — see suite/lib.sh; LIBRARIES is parsed by benchlib).
suite_build_libraries

# jnum <file> <field> <default> — one JSON field, default on any failure/null.
jnum() {
  python3 -c 'import json,sys
v=json.load(open(sys.argv[1])).get(sys.argv[2])
print(sys.argv[3] if v is None else v)' "$1" "$2" "$3" 2>/dev/null || echo "$3"
}

sweep() {   # $1=service $2=port $3=target
  local svc="$1" base="http://localhost:$2" target="$3"
  bringup_scan "$svc" "$base" "$target" || return 0

  for name in $PHASE_B_ENDPOINTS; do
    local maxrate=0 lastp99=null knee=null
    for rate in $SWEEP_RATES; do
      local f="results/raw/phaseB-$target-$name-r$rate.json"
      rm -f "$f"   # a failed leg must leave no file — never judge a stale run
      python3 phase_a.py --target "$target" --base "$base" -e "$name" \
        --rate "$rate" --dur "${SWEEP_DUR%s}" --warmup "${SWEEP_WARMUP%s}" \
        --out "$f" </dev/null >/dev/null 2>&1 || true
      [ -f "$f" ] || break
      # sustained ⇒ no dropped arrivals and it served ~all of them as 200.
      local sustained
      sustained=$(python3 -c 'import json,sys
d=json.load(open(sys.argv[1])); rate=float(sys.argv[2]); dur=float(sys.argv[3])
print(1 if d.get("dropped",1)==0 and d.get("count",0)>=int(rate*dur*0.9) else 0)' \
        "$f" "$rate" "${SWEEP_DUR%s}" 2>/dev/null || echo 0)
      # G2: the knee — the first rate whose p99 crossed the threshold, noted
      # even while the rate is still technically sustained (latency departs
      # before throughput collapses; the knee is what users would feel first).
      if [ "$knee" = "null" ]; then
        local kneenow
        kneenow=$(python3 -c 'import json,sys
d=json.load(open(sys.argv[1])); p99=d.get("p99")
print(1 if p99 is not None and p99 > float(sys.argv[2]) else 0)' \
          "$f" "${BENCH_KNEE_P99_MS:-250}" 2>/dev/null || echo 0)
        [ "$kneenow" = "1" ] && knee=$rate
      fi
      if [ "$sustained" = "1" ]; then maxrate=$rate; lastp99=$(jnum "$f" p99 null)
      else break; fi
    done
    echo "   $name: max sustainable ≈ ${maxrate} req/s (p99 ${lastp99} ms; knee ${knee} req/s @ >${BENCH_KNEE_P99_MS:-250} ms p99)"
    echo "{\"target\":\"$target\",\"endpoint\":\"$name\",\"max_rps\":$maxrate,\"p99_at_max\":$lastp99,\"knee_rate\":$knee,\"knee_p99_ms\":${BENCH_KNEE_P99_MS:-250}}" \
      > "results/raw/phaseBmax-$target-$name.json"
  done
  docker compose stop "$svc" >/dev/null 2>&1 || true
}

[ "${BENCH_ONLY:-}" != "jellyfin" ] && sweep ferrofin "$FERROFIN_HOST_PORT" ferrofin
[ "${BENCH_ONLY:-}" != "ferrofin" ]   && sweep jellyfin "$JELLYFIN_HOST_PORT" jellyfin
docker compose down -v >/dev/null 2>&1 || true

echo ">> rendering Phase B report"
VERSION=$(git -C .. describe --tags --always 2>/dev/null || echo dev)
python3 render_phases.py b "$VERSION"
echo ">> wrote results/phaseB-${VERSION}.md"
