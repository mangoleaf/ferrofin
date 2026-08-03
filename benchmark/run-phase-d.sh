#!/usr/bin/env bash
# Phase D — realistic-load comparison: a few think-time clients (phase-d.js)
# instead of the 50-VU lockstep stress. This is the "what users feel" number;
# run.sh's 50-VU table stays the stress headline.
#
#   ./run-phase-d.sh                    both servers
#   BENCH_ONLY=hermit ./run-phase-d.sh
#   PHASE_D_VUS=8 PHASE_D_DUR=120s     knobs (defaults shown)
set -euo pipefail
cd "$(dirname "$0")"
set -a; [ -f .env ] || cp .env.example .env; . ./.env; set +a
. ./_phase-common.sh
mkdir -p results/raw

export PHASE_D_VUS="${PHASE_D_VUS:-8}" PHASE_D_DUR="${PHASE_D_DUR:-120s}"

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

phase_d() {  # $1=service $2=port $3=target
  local svc="$1" base="http://localhost:$2" target="$3"
  bringup_scan "$svc" "$base" "$target" || return 0
  TARGET="$target" BASE_URL="$base" k6 run phase-d.js </dev/null
  docker compose stop "$svc" >/dev/null 2>&1 || true
}

[ "${BENCH_ONLY:-}" != "jellyfin" ] && phase_d hermit "$HERMIT_HOST_PORT" hermit
[ "${BENCH_ONLY:-}" != "hermit" ]   && phase_d jellyfin "$JELLYFIN_HOST_PORT" jellyfin
docker compose down -v >/dev/null 2>&1 || true

echo ">> rendering Phase D report"
VERSION=$(git -C .. describe --tags --always 2>/dev/null || echo dev)
node render-phase-d.mjs "$VERSION"
echo ">> wrote results/phaseD-${VERSION}.md"
