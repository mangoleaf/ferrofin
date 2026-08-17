#!/usr/bin/env bash
# DB pool-size sweep — the decisive mixed-load experiment for FERROFIN_DB_POOL.
#
# Regime matters: phase-B (open-model, single endpoint) saturates one hot query
# and rewards pool≈cores; the 50-VU mixed lockstep load (scenario.js's regime,
# what real dashboards produce) queues on connection ACQUISITION, and the right
# pool size is decided here, not there. Do not re-litigate the default from
# single-endpoint data — re-run this sweep instead.
#
#   ./pool-sweep.sh                      build + scan once, sweep POOL_SIZES
#   BENCH_SKIP_BUILD=1 ./pool-sweep.sh   reuse the existing ferrofin-bench:local image
#   POOL_SIZES="4 32" ./pool-sweep.sh    custom ladder
#
# The library is scanned ONCE (first bring-up); every pool size then recreates
# only the container (new FERROFIN_DB_POOL env) against the same data volume, so
# each extra size costs ~1 min, not a rescan.
set -euo pipefail
cd "$(dirname "$0")"
# shellcheck source=../lib.sh
source ../lib.sh
suite_load_env

POOL_SIZES=${POOL_SIZES:-"4 8 16 32 64"}
# Surface the db_pool sampler (bootstrap.rs) in container logs.
export BENCH_RUST_LOG=${BENCH_RUST_LOG:-ferrofin_server=debug,info}
# Isolated compose project + port, so a concurrent run.sh can't tear us down.
export COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-ferrofin-sweep}"
export FERROFIN_HOST_PORT="${FERROFIN_HOST_PORT:-18296}"
BASE="http://localhost:$FERROFIN_HOST_PORT"

# Shared bring-up: library list + passthrough env (suite/lib.sh).
suite_build_libraries
export BENCH_VUS BENCH_DURATION BENCH_WARMUP_SECONDS

mkdir -p results/raw

wait200() { for _ in $(seq 1 240); do curl -sf "$BASE/System/Info/Public" >/dev/null 2>&1 && return 0; sleep 1; done; echo "ferrofin never came up"; return 1; }

echo ">> clean start (fresh volume — the one scan of the sweep)"
docker compose down -v >/dev/null 2>&1 || true
first_size=${POOL_SIZES%% *}
if [ "${BENCH_SKIP_BUILD:-0}" = "1" ]; then FERROFIN_DB_POOL="$first_size" docker compose up -d ferrofin
else FERROFIN_DB_POOL="$first_size" docker compose up -d --build ferrofin; fi
wait200
echo ">> provision + scan (once)"
python3 bootstrap.py --target ferrofin --base "$BASE"

for n in $POOL_SIZES; do
  echo ">> pool=$n"
  # Recreate the container with the new env; the data volume (scanned library)
  # persists, so this is seconds, not a rescan.
  FERROFIN_DB_POOL="$n" docker compose up -d ferrofin
  wait200
  python3 pool_sweep.py --base "$BASE" --pool "$n" </dev/null
  # Keep the pool-sampler evidence for this size (acquisition-wait diagnosis).
  docker compose logs --no-log-prefix ferrofin 2>/dev/null | grep db_pool | tail -6 \
    > "results/raw/pool-$n-sampler.log" || true
done

docker compose down -v >/dev/null 2>&1 || true

SHA=$(git -C .. rev-parse --short HEAD)
# shellcheck disable=SC2086  # POOL_SIZES is intentionally word-split
python3 render_closed.py pool "$SHA" $POOL_SIZES
