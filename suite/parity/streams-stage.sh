#!/usr/bin/env bash
# STREAMS STAGE — the only stage that mounts real media, and it mounts it
# READ-ONLY, via docker-compose.streams.yml (COMPOSE_FILE overlay).
#
#   suite/parity/streams-stage.sh                    build + run
#   BENCH_SKIP_BUILD=1 suite/parity/streams-stage.sh reuse ferrofin-bench:local
#
# READ-ONLY CONTRACT (owner requirement, non-negotiable): this stage runs
# assets.py and streams.py ONLY — probes that fetch images, stream/HLS
# segments, subtitles and trickplay. No write-op probe may ever be added here;
# every write/delete journey runs in the media-less sweep stage
# (suite/parity/sweep.sh), whose compose file has no media mounts at all. The
# :ro flags are additionally enforced by suite_assert_media_readonly (resolved
# compose config) and suite_assert_running_mounts_readonly (the running
# containers' actual mounts), both of which refuse the run on any violation.
set -euo pipefail
cd "$(dirname "$0")/../perf"
# shellcheck source=../lib.sh
source ../lib.sh
export PARITY_ENV="${PARITY_ENV:-.env.loop}"
suite_load_env "$PARITY_ENV"

# The six host→container media mounts (host identity, gitignored).
if [ ! -f .env.streams ]; then
  cp .env.streams.example .env.streams
  echo "created suite/perf/.env.streams from the example — edit its paths, then rerun" >&2
  exit 1
fi
set -a
# shellcheck disable=SC1091
. ./.env.streams
set +a
# Fail FAST on a placeholder or typo'd path: docker would otherwise create the
# missing source as a root-owned empty dir and the probes degrade to
# UNRESOLVED minutes later, pointing away from the cause.
for v in STREAMS_HOT_MOVIES STREAMS_HOT_TV STREAMS_COLD_MOVIES STREAMS_COLD_TV STREAMS_COLD_MUSIC STREAMS_COLD_EDU; do
  [ -d "${!v}" ] || { echo "$v=${!v} is not a directory — fix suite/perf/.env.streams" >&2; exit 1; }
done

# Both compose files, for THIS shell only: every docker compose invocation in
# the helpers (create/seed/start, both readonly guards) sees the overlay.
export COMPOSE_FILE="docker-compose.yml:docker-compose.streams.yml"

suite_mint_device_id streams
suite_build_libraries
suite_require_media
suite_assert_media_readonly   # real media is never writable from the suite
suite_gen_fixtures
suite_require_fixtures

echo ">> starting both servers (media mounted read-only)"
docker compose down -v >/dev/null 2>&1 || true
if [ "${BENCH_SKIP_BUILD:-0}" = 1 ]; then suite_up_seeded ferrofin jellyfin livetv-source hdhomerun-source
else suite_up_seeded --build ferrofin jellyfin livetv-source hdhomerun-source; fi
suite_assert_running_mounts_readonly
trap 'docker compose down -v >/dev/null 2>&1 || true' EXIT

FERROFIN_URL="http://localhost:${FERROFIN_HOST_PORT:-18096}"
JELLYFIN_URL="http://localhost:${JELLYFIN_HOST_PORT:-18097}"
suite_wait200 "$FERROFIN_URL" ferrofin
suite_wait200 "$JELLYFIN_URL" jellyfin

STAMP="$(git rev-parse --short HEAD 2>/dev/null) $(date +%F)"
export FERROFIN_URL JELLYFIN_URL PARITY_STAMP="$STAMP"
echo ">> Layer-3 binary/asset differential"
python3 ../parity/assets.py
echo ">> Layer-3 stream-signature differential (direct / HLS / subtitles / trickplay)"
python3 ../parity/streams.py
echo ">> regenerating ledger (validates first — an unstamped verdict fails the leg)"
python3 ../parity/gen-ledger.py
