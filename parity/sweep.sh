#!/usr/bin/env bash
# Bring up both servers (reusing the benchmark docker-compose) and run the Layer-1 sweep.
# Mirrors benchmark/parity.sh's bring-up; hands off to sweep.py (the request-gen + validator).
#   parity/sweep.sh                    build + sweep
#   BENCH_SKIP_BUILD=1 parity/sweep.sh reuse the existing hermit-bench:local image (may lag HEAD)
set -euo pipefail
cd "$(dirname "$0")/../benchmark"
ENVF="${PARITY_ENV:-.env.loop}"
set -a; [ -f "$ENVF" ] || cp .env.example "$ENVF"; . "./$ENVF"; set +a

# Library list — same construction as parity.sh (LIBRARIES is parsed by sweep.py).
LIBS="["; sep=""
[ -n "${REAL_MEDIA_DIR:-}" ] && { LIBS="$LIBS${sep}{\"name\":\"Movies\",\"type\":\"movies\",\"path\":\"/media/movies-real\"}"; sep=","; }
[ -n "${REAL_TV_DIR:-}" ]    && { LIBS="$LIBS${sep}{\"name\":\"Shows\",\"type\":\"tvshows\",\"path\":\"/media/tv-real\"}"; sep=","; }
[ "${FIXTURE_MOVIES:-0}" -gt 0 ] && { LIBS="$LIBS${sep}{\"name\":\"Movies (synth)\",\"type\":\"movies\",\"path\":\"/media/synth/movies\"}"; sep=","; }
[ "${FIXTURE_SERIES:-0}" -gt 0 ] && { LIBS="$LIBS${sep}{\"name\":\"Shows (synth)\",\"type\":\"tvshows\",\"path\":\"/media/synth/tv\"}"; sep=","; }
LIBS="$LIBS]"
[ "$LIBS" = "[]" ] && { echo "No media: set REAL_MEDIA_DIR or FIXTURE_MOVIES>0 in $ENVF"; exit 1; }
export LIBRARIES="$LIBS" REAL_MEDIA_DIR REAL_TV_DIR BENCH_ADMIN_USER BENCH_ADMIN_PASSWORD JELLYFIN_IMAGE

if { [ "${FIXTURE_MOVIES:-0}" -gt 0 ] || [ "${FIXTURE_SERIES:-0}" -gt 0 ]; } && \
   [ -z "$(find fixtures/media -type f 2>/dev/null | head -1)" ]; then ./gen-fixtures.sh; fi

echo ">> starting both servers"
docker compose down -v >/dev/null 2>&1 || true
if [ "${BENCH_SKIP_BUILD:-0}" = 1 ]; then docker compose up -d hermit jellyfin
else docker compose up -d --build hermit jellyfin; fi
trap 'docker compose down -v >/dev/null 2>&1 || true' EXIT

wait200() { for _ in $(seq 1 120); do curl -sf "$1/System/Info/Public" >/dev/null 2>&1 && return; sleep 0.5; done; echo "$2 never came up"; exit 1; }
wait200 http://localhost:18096 hermit
wait200 http://localhost:18097 jellyfin

STAMP="$(git rev-parse --short HEAD 2>/dev/null) $(date +%F)"
export HERMIT_URL=http://localhost:18096 JELLYFIN_URL=http://localhost:18097 PARITY_STAMP="$STAMP"
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
