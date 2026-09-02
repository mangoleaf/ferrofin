#!/usr/bin/env bash
# Bring up both servers (reusing the perf-leg docker-compose) and run the Layer-1 sweep.
# Hands off to sweep.py (the request-gen + validator).
#   suite/parity/sweep.sh                    build + sweep
#   BENCH_SKIP_BUILD=1 suite/parity/sweep.sh reuse the existing ferrofin-bench:local image (may lag HEAD)
set -euo pipefail
cd "$(dirname "$0")/../perf"
# shellcheck source=../lib.sh
source ../lib.sh
# Exported: gen-fixtures.sh re-sources the env file named here, so the parity leg's fixture
# knobs (music, live tv) must reach it — an unexported default would let it fall back to
# .env and generate the perf fixture instead.
export PARITY_ENV="${PARITY_ENV:-.env.loop}"
suite_load_env "$PARITY_ENV"
suite_mint_device_id parity
suite_build_libraries   # LIBRARIES is parsed by sweep.py
suite_require_media
suite_assert_media_readonly   # real media is never writable from the suite
suite_gen_fixtures
suite_require_fixtures

echo ">> starting both servers"
docker compose down -v >/dev/null 2>&1 || true
if [ "${BENCH_SKIP_BUILD:-0}" = 1 ]; then suite_up_seeded ferrofin jellyfin livetv-source hdhomerun-source
else suite_up_seeded --build ferrofin jellyfin livetv-source hdhomerun-source; fi
suite_assert_running_mounts_readonly
trap 'docker compose down -v >/dev/null 2>&1 || true' EXIT

# Host ports follow the compose file's overrides, so a worktree can run its own leg
# (COMPOSE_PROJECT_NAME + FERROFIN_HOST_PORT/JELLYFIN_HOST_PORT + BENCH_CACHE_SCOPE) without
# touching the main checkout's pair.
FERROFIN_URL="http://localhost:${FERROFIN_HOST_PORT:-18096}"
JELLYFIN_URL="http://localhost:${JELLYFIN_HOST_PORT:-18097}"
suite_wait200 "$FERROFIN_URL" ferrofin
suite_wait200 "$JELLYFIN_URL" jellyfin

STAMP="$(git rev-parse --short HEAD 2>/dev/null) $(date +%F)"
export FERROFIN_URL JELLYFIN_URL PARITY_STAMP="$STAMP"
echo ">> Layer-1 breadth sweep"
python3 ../parity/sweep.py
echo ">> Layer-2 read depth (id-correlated)"
python3 ../parity/reads.py
echo ">> Layer-2 write journeys"
python3 ../parity/journeys.py
echo ">> Layer-3 binary/asset differential"
python3 ../parity/assets.py
echo ">> Layer-3 stream-signature differential (direct / HLS / subtitles / trickplay)"
python3 ../parity/streams.py
echo ">> Layer-2 push differential (server->client WebSocket messages, both servers)"
python3 ../parity/push.py
echo ">> terminal phase: restore / restart / shutdown (ends the differential)"
python3 ../parity/terminal.py
# gen-ledger.py VALIDATES BEFORE IT WRITES on every run (not only under --check):
# a row that carries a verdict without declaring which verification method earned
# it, a method outside the closed set, a method on a row with no verdict, or an
# open-work/unreviewed-flag row rendered as an accepted divergence all abort the
# leg here with nothing written. This line used to be a bare regenerate, and the
# rule it is supposed to enforce ran nowhere on any automated path.
echo ">> regenerating ledger (validates first — an unstamped verdict fails the leg)"
python3 ../parity/gen-ledger.py
# (No fingerprint capture here: merge.py's shape check runs against the committed
# suite/results/shape-baseline.json + the perf leg's own captures — a parity-leg capture
# would compare different fixtures/DB state and was never read after 15ddd6d.)
