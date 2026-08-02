#!/usr/bin/env bash
# Phase B — per-endpoint saturation sweep → max sustainable RPS.
#
# For each endpoint, drive it (open model) at increasing arrival rates until the
# server can no longer keep up — the point where k6 starts dropping arrivals
# (dropped_iterations > 0) or responses stop being 200. The last rate it served
# cleanly is that endpoint's max sustainable throughput. Scoped to a curated set
# of endpoints (a full 83-endpoint sweep would run for hours); override with
# PHASE_B_ENDPOINTS.
#
#   ./run-phase-b.sh                    both servers, curated endpoints
#   BENCH_ONLY=hermit ./run-phase-b.sh
set -euo pipefail
cd "$(dirname "$0")"
set -a; [ -f .env ] || cp .env.example .env; . ./.env; set +a
mkdir -p results/raw

SWEEP_RATES=${SWEEP_RATES:-"25 50 100 200 400 800 1600 3200 6400"}   # req/s ladder
SWEEP_DUR=${SWEEP_DUR:-8s}                                           # per-rate window (short)
SWEEP_WARMUP=${SWEEP_WARMUP:-2s}
PHASE_B_ENDPOINTS=${PHASE_B_ENDPOINTS:-"info_public user_me items_sortname items_mixed item_detail persons studios suggestions movie_recommendations items_filters2 image_primary"}
export PHASE_DUR="$SWEEP_DUR" PHASE_WARMUP="$SWEEP_WARMUP" PHASE_PRE_VUS="${PHASE_PRE_VUS:-50}" PHASE_MAX_VUS="${PHASE_MAX_VUS:-1000}"

# Library list — same construction as run.sh (parsed by k6).
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

jnum() { node -pe "require('./$1').$2" 2>/dev/null || echo "$3"; }   # $1=file $2=field $3=default

sweep() {   # $1=service $2=port $3=target
  local svc="$1" base="http://localhost:$2" target="$3"
  echo ">> [$target] up + scan"
  docker compose down -v >/dev/null 2>&1 || true
  if [ "${BENCH_SKIP_BUILD:-0}" = "1" ]; then docker compose up -d "$svc"; else docker compose up -d --build "$svc"; fi
  until curl -sf "$base/System/Info/Public" >/dev/null 2>&1; do sleep 1; done
  k6 run -e TARGET="$target" -e BASE_URL="$base" bootstrap.js

  for name in $PHASE_B_ENDPOINTS; do
    local maxrate=0 lastp99=null
    for rate in $SWEEP_RATES; do
      k6 run -e ENDPOINT="$name" -e TARGET="$target" -e BASE_URL="$base" \
        -e PHASE_RATE="$rate" -e PHASE_OUT="phaseB-$target-$name-r$rate" phase-a.js </dev/null >/dev/null 2>&1 || true
      local f="results/raw/phaseB-$target-$name-r$rate.json"
      [ -f "$f" ] || break
      local drop cnt want; drop=$(jnum "$f" dropped 1); cnt=$(jnum "$f" count 0)
      # sustained ⇒ no dropped arrivals and it served ~all of them as 200.
      want=$(awk -v r="$rate" -v d="${SWEEP_DUR%s}" 'BEGIN{print int(r*d*0.9)}')
      if [ "$drop" = "0" ] && [ "$cnt" -ge "$want" ]; then maxrate=$rate; lastp99=$(jnum "$f" p99 null)
      else break; fi
    done
    echo "   $name: max sustainable ≈ ${maxrate} req/s (p99 ${lastp99} ms)"
    echo "{\"target\":\"$target\",\"endpoint\":\"$name\",\"max_rps\":$maxrate,\"p99_at_max\":$lastp99}" \
      > "results/raw/phaseBmax-$target-$name.json"
  done
  docker compose stop "$svc" >/dev/null 2>&1 || true
}

[ "${BENCH_ONLY:-}" != "jellyfin" ] && sweep hermit   18096 hermit
[ "${BENCH_ONLY:-}" != "hermit" ]   && sweep jellyfin 18097 jellyfin
docker compose down -v >/dev/null 2>&1 || true

echo ">> rendering Phase B report"
VERSION=$(git -C .. describe --tags --always 2>/dev/null || echo dev)
node render-phase-b.mjs "$VERSION"
echo ">> wrote results/phaseB-${VERSION}.md"
