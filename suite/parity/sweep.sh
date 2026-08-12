#!/usr/bin/env bash
# Bring up both servers (reusing the perf-leg docker-compose) and run the Layer-1 sweep.
# Hands off to sweep.py (the request-gen + validator).
#   suite/parity/sweep.sh                    build + sweep
#   BENCH_SKIP_BUILD=1 suite/parity/sweep.sh reuse the existing ferrofin-bench:local image (may lag HEAD)
set -euo pipefail
cd "$(dirname "$0")/../perf"
# shellcheck source=../lib.sh
source ../lib.sh
suite_load_env "${PARITY_ENV:-.env.loop}"
suite_mint_device_id parity
suite_build_libraries   # LIBRARIES is parsed by sweep.py
suite_gen_fixtures

echo ">> starting both servers"
docker compose down -v >/dev/null 2>&1 || true
if [ "${BENCH_SKIP_BUILD:-0}" = 1 ]; then docker compose up -d ferrofin jellyfin
else docker compose up -d --build ferrofin jellyfin; fi
trap 'docker compose down -v >/dev/null 2>&1 || true' EXIT

suite_wait200 http://localhost:18096 ferrofin
suite_wait200 http://localhost:18097 jellyfin

STAMP="$(git rev-parse --short HEAD 2>/dev/null) $(date +%F)"
export FERROFIN_URL=http://localhost:18096 JELLYFIN_URL=http://localhost:18097 PARITY_STAMP="$STAMP"
echo ">> Layer-1 breadth sweep"
python3 ../parity/sweep.py
echo ">> Layer-2 read depth (id-correlated)"
python3 ../parity/reads.py
echo ">> Layer-2 write journeys"
python3 ../parity/journeys.py
echo ">> Layer-3 binary/asset differential"
python3 ../parity/assets.py
echo ">> regenerating ledger"
python3 ../parity/gen-ledger.py
echo ">> capturing Ferrofin body fingerprints (mid-run honesty baseline for merge.py)"
mkdir -p ../results/raw
python3 ../fingerprint.py capture http://localhost:18096 ../results/raw/parity-fingerprints.json || true
