#!/usr/bin/env bash
# DB pool-size sweep — the decisive mixed-load experiment for HERMIT_DB_POOL.
#
# Regime matters: phase-B (open-model, single endpoint) saturates one hot query
# and rewards pool≈cores; the 50-VU mixed lockstep load (scenario.js's regime,
# what real dashboards produce) queues on connection ACQUISITION, and the right
# pool size is decided here, not there. Do not re-litigate the default from
# single-endpoint data — re-run this sweep instead.
#
#   ./pool-sweep.sh                      build + scan once, sweep POOL_SIZES
#   BENCH_SKIP_BUILD=1 ./pool-sweep.sh   reuse the existing hermit-bench:local image
#   POOL_SIZES="4 32" ./pool-sweep.sh    custom ladder
#
# The library is scanned ONCE (first bring-up); every pool size then recreates
# only the container (new HERMIT_DB_POOL env) against the same data volume, so
# each extra size costs ~1 min, not a rescan.
set -euo pipefail
cd "$(dirname "$0")"
set -a; [ -f .env ] || cp .env.example .env; . ./.env; set +a

POOL_SIZES=${POOL_SIZES:-"4 8 16 32 64"}
# Surface the db_pool sampler (bootstrap.rs) in container logs.
export BENCH_RUST_LOG=${BENCH_RUST_LOG:-hermit_server=debug,info}
# Isolated compose project + port, so a concurrent run.sh can't tear us down.
export COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-hermit-sweep}"
export HERMIT_HOST_PORT="${HERMIT_HOST_PORT:-18296}"
BASE="http://localhost:$HERMIT_HOST_PORT"

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
export LIBRARIES="$LIBS" REAL_MEDIA_DIR REAL_TV_DIR BENCH_ADMIN_USER BENCH_ADMIN_PASSWORD EXPECTED_ITEMS
export BENCH_VUS BENCH_DURATION BENCH_WARMUP_SECONDS

mkdir -p results/raw

wait200() { for _ in $(seq 1 240); do curl -sf "$BASE/System/Info/Public" >/dev/null 2>&1 && return 0; sleep 1; done; echo "hermit never came up"; return 1; }

echo ">> clean start (fresh volume — the one scan of the sweep)"
docker compose down -v >/dev/null 2>&1 || true
first_size=${POOL_SIZES%% *}
if [ "${BENCH_SKIP_BUILD:-0}" = "1" ]; then HERMIT_DB_POOL="$first_size" docker compose up -d hermit
else HERMIT_DB_POOL="$first_size" docker compose up -d --build hermit; fi
wait200
echo ">> provision + scan (once)"
TARGET=hermit BASE_URL="$BASE" k6 run bootstrap.js

for n in $POOL_SIZES; do
  echo ">> pool=$n"
  # Recreate the container with the new env; the data volume (scanned library)
  # persists, so this is seconds, not a rescan.
  HERMIT_DB_POOL="$n" docker compose up -d hermit
  wait200
  POOL="$n" BASE_URL="$BASE" k6 run pool-sweep.js </dev/null
  # Keep the pool-sampler evidence for this size (acquisition-wait diagnosis).
  docker compose logs --no-log-prefix hermit 2>/dev/null | grep db_pool | tail -6 \
    > "results/raw/pool-$n-sampler.log" || true
done

docker compose down -v >/dev/null 2>&1 || true

SHA=$(git -C .. rev-parse --short HEAD)
# shellcheck disable=SC2086  # POOL_SIZES is intentionally word-split
node render-pool-sweep.mjs "$SHA" $POOL_SIZES
