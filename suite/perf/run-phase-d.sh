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
# shellcheck source=../lib.sh
source ../lib.sh
suite_load_env
. ./_phase-common.sh
mkdir -p results/raw

export PHASE_D_VUS="${PHASE_D_VUS:-8}" PHASE_D_DUR="${PHASE_D_DUR:-120s}"

# Shared bring-up: library list + passthrough env (suite/lib.sh).
suite_build_libraries

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
